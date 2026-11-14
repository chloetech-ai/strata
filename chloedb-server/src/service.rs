//! The operations both front ends serve, over a local database or, as a
//! coordinator, over shard servers.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use chloedb::db::{Db, Footprints, PreparedWrite, Source};
use chloedb::embed::Embedder;
use chloedb::ingest::{Format, Rows};
use chloedb::query::exec::{Engine, ExecOptions, Expansions, Leaf, ResultSet};
use chloedb::query::memory::MemoryBudget;
use chloedb::query::stats::TermStats;
use chloedb::query::ir::Plan;
use chloedb::query::model::Models;
use chloedb::query::paths::{Call, Paths};
use chloedb::query::sql::{ident, quoted};
use chloedb::query::function::FunctionSet;
use chloedb::query::{bind_command, compound, ops, profile, steps, Command, Params, Select};
use chloedb::rank_log::{self, Log};
use chloedb::schema::{Alter, Schema};
use chloedb::storage::{CheckpointOptions, CheckpointStats, Layout, RankProfile, Snapshot, TableTxn, WriteTxn, FUNCTIONS, SOURCES};
use chloedb::text::Synonyms;
use chloedb::value::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Snapshot leases held at once at most; a request for one more is
/// refused (429) until some expire.
pub const MAX_LEASES: usize = 10_000;

/// A memory budget of `bytes` for the queries of a server
/// ([`Service::query_memory`]), whose refusals name `--query-memory-mb`.
pub fn query_budget(bytes: u64) -> Arc<MemoryBudget> {
    MemoryBudget::named(bytes, "the server lets queries hold at once (--query-memory-mb)")
}

use crate::coord::Coordinator;
use crate::idem;
use crate::replication::{Acks, Follow, Primary, PrimaryOptions, READ_WAIT};
use crate::cores::slots::{self as core, Moved};
use crate::slots::{self, SlotMap};
use crate::metrics::{self, Family, Metrics};
use crate::alerts::Alerts;
use crate::keys::{self, Audit, Keys, Principal, Scope};
use crate::telemetry::{self, LiveQueries, QueryLog, Sample, Series};
use crate::{json as js, obs, pb, ApiError};

pub(crate) mod arrivals;
mod failover;
pub mod feed;
mod moving;
mod scroll;
pub mod scrub;
mod switch;

pub use failover::tuned as failover_tuned;
pub(crate) use moving::Moving;
pub use feed::MARK;
pub use moving::FREEZE_TTL;
pub use scroll::{owner as scroll_owner, ScrollAsk, MAX_KEEP_ALIVE, MAX_SCROLL_ROWS};

tokio::task_local! {
    /// The cluster map epoch the request being served routes by, as
    /// [`Service::check_epoch`] checked it (0: none): a read takes its
    /// version and a write checks its keys against the map then.
    pub static ROUTED: std::cell::Cell<u64>;
}

/// A snapshot kept for the queries that name its lease, until `until`,
/// and the typo expansions they found on it.
struct Lease {
    snap: Snapshot,
    until: Instant,
    expansions: Arc<Expansions>,
}

/// The deadline a request asked for (`timeout_ms`), when shorter than the
/// server's default, and whether it passed while a shard was asked: a
/// coordinator then neither avoids the copies it asked nor counts their
/// failure, and the request is logged as the client's deadline, not a
/// fault ([`client_deadline`]).
#[derive(Default)]
pub struct Deadline {
    asked_ms: std::cell::Cell<Option<u64>>,
    passed: std::cell::Cell<bool>,
    /// A shard's part whose deadline is the client's own, as the
    /// coordinator says ([`CLIENT_DEADLINE`]).
    clients: bool,
}

/// The gRPC metadata by which a coordinator says that a shard's part runs
/// to its client's own deadline (`timeout_ms` below the server's default),
/// so that the shard counts and logs a deadline that passes as the
/// client's, not as its own failure.
pub const CLIENT_DEADLINE: &str = "x-strata-deadline";

impl Deadline {
    /// A shard's part's deadline, its client's own when `clients`.
    pub fn of_part(clients: bool) -> Deadline {
        Deadline { clients, ..Default::default() }
    }
}

/// Whether the shard's part being served runs to its client's own
/// deadline ([`CLIENT_DEADLINE`]).
pub fn part_of_clients_deadline() -> bool {
    DEADLINE.try_with(|d| d.clients).unwrap_or(false)
}

tokio::task_local! {
    /// The request's [`Deadline`] (HTTP requests run in one).
    pub static DEADLINE: Deadline;
}

tokio::task_local! {
    /// The cluster whose changes the request writes, when a follow process
    /// sends it ([`FOLLOW_HEADER`], with the cluster's token): a cluster
    /// that follows that one takes them ([`Service::follows`]).
    pub static FOLLOW: Option<Arc<str>>;
}

tokio::task_local! {
    /// Whether the request is the cluster's own (its token: a coordinator's
    /// call to its shards, a follow process's write, or the server's own
    /// work), which may write the server's own marks
    /// ([`feed::own_mark`]: a hold of its writes, a promotion).
    pub static OWN: bool;
}

/// The tables statement `command` changes: the one it writes, makes,
/// alters or drops, or whose rank profile it makes or drops.
fn changed_tables(command: &Command) -> Vec<&str> {
    match command {
        Command::Write { table, .. } | Command::CreateRankProfile { table, .. } => vec![table.as_str()],
        Command::CreateTable { schema, .. } => vec![schema.table.as_str()],
        Command::DropTable { tables, .. } => tables.iter().map(String::as_str).collect(),
        Command::AlterTable { table: Some(t), .. } | Command::DropRankProfile { table: Some(t), .. } => vec![t.as_str()],
        _ => Vec::new(),
    }
}

/// Whether the request being served is the cluster's own ([`OWN`]).
pub fn own() -> bool {
    OWN.try_with(|o| *o).unwrap_or(false)
}

/// The header (and gRPC metadata key) by which a follow process names
/// the cluster it follows ([`FOLLOW`]); a coordinator passes it on to its
/// shards.
pub const FOLLOW_HEADER: &str = "x-strata-follow";

/// The cluster the request being served writes the changes of
/// ([`FOLLOW`]).
pub fn following() -> Option<Arc<str>> {
    FOLLOW.try_with(Clone::clone).ok().flatten()
}

/// Whether `name` may name a cluster to follow: 1 to 64 letters, digits,
/// `.`, `_` or `-`.
pub fn follow_name_ok(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The client's own deadline in ms, when the request being served asked
/// for one below the server's default.
pub fn client_deadline() -> Option<u64> {
    DEADLINE.try_with(|d| d.asked_ms.get()).ok().flatten()
}

/// Notes that the client's own deadline passed.
pub fn deadline_passed() {
    let _ = DEADLINE.try_with(|d| d.passed.set(true));
}

/// Whether the request's own deadline passed, and that deadline.
pub fn deadline_of_request() -> Option<u64> {
    DEADLINE.try_with(|d| d.passed.get().then(|| d.asked_ms.get()).flatten()).ok().flatten()
}

/// The epoch the request being served routes by ([`ROUTED`]).
fn routed() -> u64 {
    ROUTED.try_with(std::cell::Cell::get).unwrap_or(0)
}

/// Binds one SELECT into its plan, and gives back what the executor runs
/// of it: every query runs as a plan ([`chloedb::query::ir`]).
fn bind(sql: &str, schema: &Schema, params: &Params) -> chloedb::Result<Select> {
    Plan::bind(sql, schema, params)?.to_select(schema)
}

pub enum Backend {
    Local(Arc<Db>),
    Cluster(Arc<Coordinator>),
}

/// How long a coordinator keeps the gazetteers of query understanding
/// before it reads them from its shards again.
pub const GAZETTEER_TTL: Duration = Duration::from_secs(60);

/// The most expired rows one transaction of the expirer deletes
/// ([`Service::expire_due`]).
pub const EXPIRE_BATCH: usize = 10_000;

/// The most keys one read by key takes ([`Service::get_rows`]).
pub const MAX_GET_KEYS: usize = 10_000;

/// Rows read by key ([`Service::get_rows`]): per key, in their order, the
/// row's values (of `columns`, as `SELECT *` names them) and the number
/// of the write that last changed it, or `None` for a key no live row has.
#[derive(Clone, Debug)]
pub struct KeyRows {
    pub table: String,
    pub columns: Vec<String>,
    pub rows: Vec<Option<(Vec<Value>, u64)>>,
}

/// A SELECT to run.
#[derive(Clone, Default)]
pub struct QueryReq {
    pub sql: String,
    pub params: Params,
    /// 0: the server's default.
    pub timeout_ms: u64,
    /// Return results a coordinator can merge ([`ExecOptions::shard`]).
    pub shard: bool,
    /// Statistics summed over shards, for global BM25 scores.
    pub stats: Option<TermStats>,
    /// A snapshot lease; 0 for the latest version.
    pub snapshot: u64,
    /// From a coordinator: answer without shards that cannot answer
    /// (`None`: its default, [`crate::coord::CoordOptions::partial`]).
    pub partial: Option<bool>,
    /// A plan to run instead of `sql` (which then only names it in logs):
    /// a leaf of a coordinator's compound query.
    pub plan: Option<Arc<Plan>>,
    /// Keep the snapshot read, `snapshot` or a new lease, for a fetch that
    /// follows ([`Answer::lease`]).
    pub hold: bool,
    /// End the lease `snapshot` once answered.
    pub release: bool,
    /// The writes the query must see ([`After`]).
    pub after: After,
    /// Measure each step's CPU time ([`ExecOptions::profile`]).
    pub profile: bool,
    /// Not kept among the latest queries, nor in the statements'
    /// statistics: a run of a CPU profile's query ([`crate::profiler`]),
    /// which runs it thousands of times. Still counted in the metrics.
    pub quiet: bool,
    /// A read of the server's own, not a client's: the rows a classifier
    /// trains on, `chunks_of` splits or mining reads. Not held to
    /// `--max-rows` (through a coordinator, a shard still holds a leaf to
    /// [`compound::MAX_ROWS`]).
    pub own_read: bool,
    /// A leaf of a compound query that DataFusion runs: its rows as Arrow
    /// columns when there are more than this many ([`Leaf`]), which a
    /// coordinator asks its shards for (protocol 10,
    /// [`crate::grpc::ARROW_PROTOCOL`]) and a shard answers
    /// ([`Answer::arrow`]) when the coordinator puts its rows after the
    /// others' ([`chloedb::cluster::concatenates`]).
    pub arrow: Option<usize>,
    /// Count the answer's rows as retrievals of their documents
    /// ([`crate::reads`]): a client's query, which a coordinator counts by
    /// the replica that answered each row.
    pub reads: bool,
    /// `stats` are of this server's version `stats_version`
    /// ([`Service::stats_version`]), which a coordinator kept: run only on
    /// that version, else answer [`Answer::stale`]. 0: any version.
    pub stats_version: u64,
    /// Start a scroll ([`scroll`]): answer the first page, and keep the
    /// rest for [`Service::scroll_next`] ([`Answer::cursor`]).
    pub scroll: Option<ScrollAsk>,
    /// Whose model budget the query's `llm()` questions take from
    /// ([`crate::budget`]); `None`: the server's own query, not counted.
    pub who: Option<Principal>,
    /// From a coordinator that fuses a rank-safe boost from the rows each
    /// shard's bound found alone ([`crate::grpc::BOOSTS_PROTOCOL`]): the
    /// shard leaves out the boost's inputs' top rows
    /// ([`ExecOptions::boost_inputs`]). False answers them, for any other.
    pub boosts_alone: bool,
    /// Answer without the timed steps (`stats.steps`), which are kept in
    /// the list of queries all the same: a client's query that did not ask
    /// for them, and a coordinator's shards for it. Most of an answer's
    /// bytes are its steps, and a coordinator spends a sixth of its CPU
    /// reading its shards' and writing them out again.
    pub skip_steps: bool,
    /// What names the query for the answer cache ([`crate::answers`]): its
    /// SQL and what a coordinator sent with it (plan, statistics), made
    /// where the request arrived; its parameters and this server's
    /// settings are added here. `None`: not cached.
    pub cache_key: Option<crate::answers::Key>,
}

/// The writes a read must see, so that a client reads its own writes
/// from any copy: the sequence number a server answered its last write
/// with (`min_seq_no`), or through a coordinator, the number each shard
/// answered (`min_seq_nos`, 0 for a shard not written). A replica waits
/// until it has applied that write, up to [`crate::replication::READ_WAIT`],
/// and refuses the read otherwise (503), so that a coordinator asks the
/// shard's primary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum After {
    #[default]
    Any,
    Seq(u64),
    Shards(Vec<u64>),
}

impl After {
    /// From a request's `min_seq_no` and `min_seq_nos` (0 and empty: not
    /// given), which are not both given.
    pub fn of(min_seq_no: u64, min_seq_nos: Vec<u64>) -> Result<After, ApiError> {
        match (min_seq_no, min_seq_nos.is_empty()) {
            (0, true) => Ok(After::Any),
            (n, true) => Ok(After::Seq(n)),
            (0, false) => Ok(After::Shards(min_seq_nos)),
            _ => Err(ApiError::bad("a read takes min_seq_no (a server's sequence number) or min_seq_nos (a coordinator's, one per shard), not both")),
        }
    }

    /// The number a read on a server must see (0: none).
    fn seq(&self) -> Result<u64, ApiError> {
        match self {
            After::Any => Ok(0),
            After::Seq(n) => Ok(*n),
            After::Shards(_) => Err(ApiError::bad("min_seq_nos is a coordinator's, one number per shard; a server takes min_seq_no, the number its write answered")),
        }
    }
}

pub struct Answer {
    pub rs: ResultSet,
    pub took_ms: f64,
    pub kind: &'static str,
    /// Shards a coordinator answered without (see [`QueryReq::partial`]).
    pub missing: Vec<String>,
    /// The lease on the snapshot read, with [`QueryReq::hold`]; else 0.
    pub lease: u64,
    /// A shard's rows as an Arrow IPC stream ([`QueryReq::arrow`],
    /// `QueryResponse.arrow`), `rs` then holding none; empty otherwise.
    pub arrow: Vec<u8>,
    /// The query did not run: its [`QueryReq::stats_version`] is not the
    /// version it would read.
    pub stale: bool,
    /// A scroll's cursor, while rows are left to read ([`QueryReq::scroll`]).
    pub cursor: Option<String>,
}

/// An answer of no rows, of a query that did not run.
fn no_rows() -> ResultSet {
    ResultSet { columns: Vec::new(), rows: Vec::new(), keys: Vec::new(), scores: Vec::new(), parts: None, stats: Default::default() }
}

/// A model the command center asked this server to fetch or convert
/// ([`Service::start_model_job`]), by the model it makes.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ModelJob {
    /// `fetch` or `convert`.
    pub action: &'static str,
    /// The model converted (`convert`).
    pub from: Option<String>,
    /// `running`, `done` or `failed`.
    pub state: &'static str,
    pub error: Option<String>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

/// An embedded VECTOR field ([`chloedb::Embed`]) and its model, which
/// computes the vectors writes leave out and embeds `{"embed": "text"}`
/// parameters.
#[derive(Clone)]
pub struct FieldModel {
    pub table: String,
    pub field: String,
    pub spec: chloedb::Embed,
    /// The model, or why it did not open (then writes must supply the
    /// vectors, and text cannot be searched against the field).
    pub model: Result<Arc<dyn Embedder>, String>,
}

/// The filling in of the vectors an embedded field's rows were stored
/// without, while it had no model open ([`Service::keep_models`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Backfill {
    /// `running`, `done` or `failed` (tried again five minutes later).
    pub state: &'static str,
    /// Rows found without a vector as it started, those whose sources are
    /// all empty included (they get none).
    pub missing: u64,
    /// Rows given their vector.
    pub embedded: u64,
    pub error: Option<String>,
}

/// Rows a backfill write fills in.
const BACKFILL_BATCH: usize = 256;

/// The agent's model settings ([`crate::agent`]).
#[derive(Clone)]
pub struct AgentConfig {
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: String,
}

/// The cluster a server is of, as people name it (`--cluster-name`,
/// `--environment`), and where its command center is (`--public-url`),
/// for links.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub name: Option<String>,
    pub environment: Option<String>,
    pub public_url: Option<String>,
}

impl Identity {
    pub fn json(&self) -> Json {
        json!({"name": self.name, "environment": self.environment, "public_url": self.public_url})
    }
}

/// The service levels fleet health checks against (`--slo-error-ratio`,
/// `--slo-p99-ms`).
#[derive(Clone, Copy, Debug)]
pub struct Slo {
    pub error_ratio: f64,
    pub p99_ms: f64,
}

impl Default for Slo {
    fn default() -> Slo {
        Slo { error_ratio: 0.01, p99_ms: 250.0 }
    }
}

/// Where a server runs: its name if given (`--node-name`), the machine
/// and zone (`CHLOEDB_NODE` or `NODE_NAME`, `CHLOEDB_ZONE`), and its image
/// (`CHLOEDB_IMAGE`, which the operator sets; its digest when it pins one,
/// or `CHLOEDB_IMAGE_DIGEST`).
#[derive(Clone, Debug, Default)]
pub struct Place {
    pub name: Option<String>,
    pub node: Option<String>,
    pub zone: Option<String>,
    pub image: Option<String>,
    pub digest: Option<String>,
}

impl Place {
    /// From the environment, with `name` given or not.
    pub fn from_env(name: Option<String>) -> Place {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let image = var("CHLOEDB_IMAGE");
        let digest = var("CHLOEDB_IMAGE_DIGEST").or_else(|| image.as_deref().and_then(crate::operator::upgrade::digest_of));
        // On Kubernetes the operator names the pod (`CHLOEDB_POD`).
        Place { name: name.or_else(|| var("CHLOEDB_POD")), node: var("CHLOEDB_NODE").or_else(|| var("NODE_NAME")), zone: var("CHLOEDB_ZONE"), image, digest }
    }

    /// Its version: this build's, and the image's.
    pub fn version(&self) -> Json {
        let mut v = json!({"version": env!("CARGO_PKG_VERSION")});
        if let Some(i) = &self.image {
            v["image"] = json!(i);
        }
        if let Some(d) = &self.digest {
            v["digest"] = json!(d);
        }
        v
    }
}

/// The log filter as last set ([`Service::set_log_level`]).
#[derive(Clone, Debug, Default)]
pub struct LogLevel {
    /// The filter to go back to, when this one expires.
    pub previous: Option<String>,
    pub set_by: Option<String>,
    pub set_ms: Option<u64>,
    pub expires_ms: Option<u64>,
    pub generation: u64,
}

/// How long a log filter set holds by default: 30 minutes.
pub const LOG_FILTER_S: u64 = 1800;

pub struct Service {
    pub backend: Backend,
    pub schema: Schema,
    pub metrics: Metrics,
    pub agent: AgentConfig,
    /// The model calls each API key may make a day ([`crate::budget`]).
    pub model_budget: Arc<crate::budget::ModelBudget>,
    // ----- core: identity, service levels, place (command center rebuild)
    pub identity: Identity,
    pub slo: Slo,
    pub place: Place,
    /// Proxies whose `X-Forwarded-For` names the client (`--trust-proxy`):
    /// addresses and prefix lengths.
    pub trusted_proxies: Vec<(std::net::IpAddr, u8)>,
    /// The apps the command center shows (`--app`), which `/v1/apps/<name>`
    /// reaches ([`crate::apps`]).
    pub apps: crate::apps::Apps,
    /// The tenants this server opens from an object store (`--tenants`;
    /// experimental, [`crate::tenants`]).
    pub tenants: Option<Arc<crate::tenants::Tenants>>,
    /// Fleet health's memory ([`crate::fleet`]).
    pub fleet: crate::fleet::Fleet,
    /// The coordinators this server knows ([`crate::peers`]).
    pub peers: crate::peers::Peers,
    /// The repository the version picker lists (`--image-repository`).
    pub image_repository: String,
    /// The rollout the coordinator watches run: its version, when it saw
    /// it start, and how many copies were stale then.
    pub rollout_seen: std::sync::Mutex<Option<(String, u64, u64)>>,
    /// The last rollout's pace: seconds a copy.
    pub rollout_pace_s: std::sync::Mutex<Option<f64>>,
    /// The log filter set for a while: the filter before it, who set it,
    /// until when, and a number each change takes
    /// ([`Self::set_log_level`]).
    pub log_level: Arc<std::sync::Mutex<LogLevel>>,
    /// `GET /v1/index/tables`'s answer, and when ([`crate::inspect::tables`]).
    pub tables_kept: tokio::sync::Mutex<Option<(std::time::Instant, Json)>>,
    /// Each table's counts at the looks of the last minutes, for its rates
    /// ([`crate::inspect::tables`]).
    pub tables_counts: std::sync::Mutex<crate::inspect::Counts>,
    /// What happened, in order ([`crate::events`]).
    pub events: Arc<crate::events::Events>,
    // ----- end core
    /// The `StrataCluster` that runs this coordinator on Kubernetes, if one
    /// does ([`crate::operator::kube::Link`]).
    pub kube: Option<crate::operator::kube::Link>,
    /// The queries running now ([`telemetry::LiveQueries`]).
    pub live: Arc<LiveQueries>,
    /// The latest and slow queries, and statistics per statement.
    pub query_log: QueryLog,
    /// How often each table's documents are retrieved and its keys asked
    /// for ([`crate::reads`]).
    pub reads: crate::reads::Reads,
    /// Rates and gauges every few seconds ([`Self::sample`]).
    pub series: Series,
    /// When the server started.
    pub started: std::time::SystemTime,
    /// The API keys ([`crate::keys`]), the audit log, and the alert rules
    /// the sampler checks.
    pub keys: Arc<Keys>,
    pub audit: Audit,
    pub alerts: Alerts,
    /// Contain rules, applied at admission ([`crate::contain`]): alerts agent.
    pub contain: crate::contain::Contain,
    /// The on-call agent's proposals, waiting for an admin ([`crate::oncall`]).
    pub oncall: crate::oncall::Desk,
    /// Connectors: other systems' data into tables ([`crate::connectors`]).
    pub connectors: crate::connectors::Connectors,
    /// The cloud view's AWS credentials and machine catalogues ([`crate::aws`]).
    pub aws: Arc<crate::aws::Aws>,
    /// The cloud view's DigitalOcean token and size catalogues
    /// ([`crate::digitalocean`]).
    pub digitalocean: Arc<crate::digitalocean::DigitalOcean>,
    /// The clusters the cloud view made, and the loop that sees each one
    /// through ([`crate::cloud::clusters`]).
    pub clouds: Arc<crate::cloud::clusters::Clusters>,
    /// The other clusters this server's command center reaches, with
    /// their keys ([`crate::hub`]).
    pub hub: Arc<crate::hub::Hub>,
    /// The server's log store ([`crate::logstore`]): its log lines and the
    /// queries it answered, in a Strata database of their own (`None`:
    /// `--log-store off`, or a coordinator without one).
    pub log_store: Option<Arc<crate::logstore::LogStore>>,
    /// The embedded VECTOR fields' models, of every table
    /// ([`Service::open_models`], [`Service::models`]).
    models: RwLock<Arc<Vec<FieldModel>>>,
    /// Where [`Service::open_models`] opened them, for tables created
    /// later, and where ranking models are.
    models_dir: Option<PathBuf>,
    /// Whether [`Service::keep_models`] fills in the vectors of rows stored
    /// without one (`--no-backfill`: not), and fetches the models Strata
    /// knows that a field names and the models directory lacks
    /// (`--no-fetch-models`: not).
    pub backfill: bool,
    pub fetch_models: bool,
    /// Told when the models may have changed: a table created or altered,
    /// a model fetched, a replica promoted.
    models_changed: tokio::sync::watch::Sender<u64>,
    /// Each embedded field's backfill, by table (lower case) and field.
    backfills: Mutex<HashMap<(String, String), Backfill>>,
    /// The models a fetch was tried for, once each.
    fetch_tried: Mutex<HashSet<String>>,
    /// The fetches and conversions asked for over HTTP, by the model each
    /// makes (lower case).
    model_jobs: Mutex<HashMap<String, ModelJob>>,
    /// A coordinator's ranking models ([`Service::open_ranking_models`]);
    /// a database keeps its own.
    ranking: Models,
    /// Default query deadline.
    pub timeout: Duration,
    /// The longest deadline a request may ask for (`timeout_ms`).
    pub max_timeout: Duration,
    /// Queries and writes that take this long or longer are logged as
    /// slow (`None`: none are), unless changed while the server runs
    /// ([`Self::slow_after`]).
    pub slow: Option<Duration>,
    /// Profiling settings changed at run time: the slow threshold, and
    /// whether every query's CPU time is measured.
    pub profiling: telemetry::Profiling,
    /// The most rows a query may reach, OFFSET included
    /// ([`ExecOptions::max_rows`]; `None`: no limit).
    pub max_rows: Option<usize>,
    /// The most groups a query may make ([`ExecOptions::max_groups`]).
    pub max_groups: Option<usize>,
    /// The most rows a cross-encoder's rerank phase may score
    /// (`--max-rerank`; `None`: no limit).
    pub max_rerank: Option<usize>,
    /// The memory queries may hold at once ([`ExecOptions::memory`];
    /// `None`: not counted).
    pub query_memory: Option<Arc<MemoryBudget>>,
    /// The engine of compound queries' joins, groups and sorts, here or
    /// above a coordinator's shards ([`Engine`]; `CHLOEDB_ENGINE`, by
    /// default `auto`).
    pub engine: Engine,
    /// The longest statement a request may send, in bytes, and the most
    /// elements (words, values and symbols) it may hold
    /// ([`chloedb::query::parse::check_size`]; `--max-statement-bytes`,
    /// `--max-statement-elements`; 0: no bound).
    pub max_statement_bytes: usize,
    pub max_statement_elements: usize,
    /// The most write requests in the write path at once (embedding,
    /// waiting for the writer or being applied); more are refused with 429.
    pub max_queued_writes: usize,
    /// The largest body `/v1/write` and `/v1/import` (and a gRPC message)
    /// may have, in bytes; other routes take [`SMALL_BODY`].
    pub max_body: usize,
    /// How long a table OTLP's logs make keeps its rows, in seconds
    /// (`--otlp-retain`; `None`: for ever).
    pub otlp_retain: Option<i64>,
    pub lease_ttl: Duration,
    leases: Mutex<HashMap<u64, Lease>>,
    next_lease: AtomicU64,
    /// The open scrolls, by cursor ([`scroll`]).
    scrolls: Mutex<HashMap<String, scroll::Cursor>>,
    /// The rows (or rows' places) the open scrolls may hold between them
    /// (`--max-scroll-rows`).
    pub max_scroll_rows: usize,
    /// Random for each run of the server: what a snapshot's statistics
    /// version is drawn from with its text statistics' generation
    /// ([`Self::stats_version`]), so that versions from before a restart
    /// are not taken for later ones.
    stats_nonce: u64,
    /// Queries that did not run, sent statistics of another version
    /// ([`QueryReq::stats_version`]).
    stale_stats: AtomicU64,
    /// Queries allowed to run at once; the rest wait until their deadline.
    admission: Arc<Semaphore>,
    pub max_concurrent: usize,
    /// How long a client's query waits for a slot before it is refused
    /// with 429 (`--admission-wait-ms`); `None`: until its deadline. A
    /// query that waits its whole deadline in a queue only holds a client
    /// longer, so past saturation a short wait keeps the latency of the
    /// queries that run near the latency of an idle server, and the
    /// throughput at its peak. A shard's calls from its coordinator always
    /// wait until their deadline: the coordinator's own slots bound them.
    pub admission_wait: Option<Duration>,
    /// Answers kept between queries ([`crate::answers`]; `None`: none).
    pub answers: Option<Arc<crate::answers::AnswerCache>>,
    /// Texts embedded at once (one per core): writes' and queries'.
    embedding: Arc<Semaphore>,
    /// Models queries named in `embed('model', ..)`, opened from the models
    /// directory at their first query ([`Service::named_model`]).
    named_models: Mutex<HashMap<String, Arc<dyn Embedder>>>,
    cores: usize,
    /// Queries running, counted until their work ends ([`Running`]).
    running: Arc<AtomicUsize>,
    /// The queue of a local database's writer thread ([`writer`]).
    writes: Option<mpsc::Sender<Queued>>,
    writer_state: Arc<WriterState>,
    /// Idempotency keys of the writes it applied ([`crate::idem`]).
    idempotency: Arc<idem::Keys>,
    /// The synonym list `synonyms => 1` searches use, and its file.
    synonyms: RwLock<Option<Arc<Synonyms>>>,
    /// A coordinator's gazetteers for query understanding
    /// ([`Service::gazetteers`]), with when they were read.
    gazetteers: std::sync::Mutex<HashMap<(String, String, chloedb::text::Analyzer), (Instant, Arc<chloedb::query::understand::Gazetteer>)>>,
    synonyms_file: Option<PathBuf>,
    /// Where `POST /v1/admin/checkpoint` writes checkpoints.
    backup_dir: Option<PathBuf>,
    /// Where `POST /v1/admin/slices` writes slices, one directory a table,
    /// which `GET /v1/slices/...` serves ([`chloedb::slice`]), and how
    /// devices use them ([`crate::slices`]).
    slices: Option<Arc<crate::slices::Slices>>,
    /// The slot map a coordinator gave this shard and its place in it
    /// ([`Service::set_slots`]), and the file that keeps them (a new map
    /// is set while its lock is held).
    slots: RwLock<Option<(SlotMap, usize)>>,
    /// The oldest map epoch under which this shard holds the slots it
    /// holds now: a coordinator routing by a map that old still reads and
    /// writes here correctly ([`Service::check_epoch`]).
    slots_since: AtomicU64,
    slots_file: tokio::sync::Mutex<Option<PathBuf>>,
    /// Its part in moving slots between shards ([`moving`]).
    pub(crate) moving: Moving,
    /// The cluster it follows, as last read ([`Self::follows`]).
    follows: feed::Follows,
    /// Its part in replication ([`crate::replication`]), which a replica's
    /// promotion changes ([`Self::promote`]).
    replication: RwLock<Replication>,
    /// How a replica ships once promoted, but for its replicas and
    /// `--write-acks` (none: as [`PrimaryOptions::default`]).
    pub promote_options: Option<PrimaryOptions>,
    /// Held while a promotion or a change of its part runs.
    promoting: tokio::sync::Mutex<()>,
    /// Where its part in replication is kept (`<db>.replication`).
    replication_file: Option<PathBuf>,
    /// How long a replica hears nothing from its primary before it asks
    /// its shard's other copies to elect it (`--failover-after-ms`; zero:
    /// never), as [`Self::start_failover`] does.
    pub failover_after: Duration,
    /// Scrubbing: how often, how fast, and what the last scrub found
    /// (`--scrub-every-s`, [`Self::start_scrubs`]).
    pub scrubs: scrub::Scrubs,
    /// The command center's Simulator: its test run and the reports kept
    /// ([`crate::traffic::run`]).
    pub simulator: crate::traffic::run::Simulator,
}

/// A server's part in replication.
#[derive(Clone, Default)]
pub enum Replication {
    #[default]
    None,
    /// Ships its writes to its replicas (`--replicas`).
    Primary(Arc<Primary>),
    /// Applies its primary's writes and takes none from clients
    /// (`--replica`).
    Replica(Arc<Follow>),
}

impl Service {
    pub fn new(backend: Backend, schema: Schema, agent: AgentConfig, max_concurrent: usize, timeout: Duration) -> Service {
        // Steps count from this moment, which must come before any
        // request's start for them to line up with its trace.
        steps::epoch();
        // With the counting allocator, a profiled query's steps carry the
        // bytes they allocated, and the heap's peak is kept.
        if crate::heap::installed() {
            steps::count_allocations(crate::heap::thread_allocated);
            crate::heap::watch();
        }
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        // core: a coordinator's events come from its cluster snapshots.
        let events = Arc::new(crate::events::Events::new());
        if let Backend::Cluster(c) = &backend {
            let ev = events.clone();
            c.observe(move |before, after| ev.observe(before, after));
        }
        let idempotency = Arc::new(idem::Keys::default());
        let writer_state = Arc::new(WriterState::default());
        let writes = match &backend {
            Backend::Local(db) => {
                let (tx, rx) = mpsc::channel();
                let (db, state) = (db.clone(), writer_state.clone());
                std::thread::Builder::new().name("chloedb-writer".into()).spawn(move || writer(&db, rx, &state)).expect("spawn the writer thread");
                Some(tx)
            }
            Backend::Cluster(_) => None,
        };
        Service {
            backend,
            schema,
            metrics: Metrics::default(),
            agent,
            model_budget: Default::default(),
            identity: Identity::default(),
            slo: Slo::default(),
            place: Place::from_env(None),
            trusted_proxies: Vec::new(),
            apps: Default::default(),
            tenants: None,
            fleet: Default::default(),
            peers: Default::default(),
            image_repository: crate::operator::upgrade::REPOSITORY.into(),
            rollout_seen: Default::default(),
            rollout_pace_s: Default::default(),
            tables_kept: Default::default(),
            tables_counts: Default::default(),
            log_level: Default::default(),
            events,
            kube: None,
            live: Arc::new(LiveQueries::default()),
            query_log: QueryLog::default(),
            reads: crate::reads::Reads::default(),
            series: Series::default(),
            started: std::time::SystemTime::now(),
            keys: Arc::new(Keys::memory()),
            audit: Audit::new(None).expect("an audit log in memory"),
            alerts: Alerts::new(None, None).expect("the default alert rules"),
            contain: Default::default(),
            oncall: Default::default(),
            connectors: Default::default(),
            aws: Arc::new(crate::aws::Aws::memory()),
            digitalocean: Arc::new(crate::digitalocean::DigitalOcean::memory()),
            clouds: Arc::new(crate::cloud::clusters::Clusters::memory()),
            hub: Arc::new(crate::hub::Hub::memory()),
            log_store: None,
            models: RwLock::new(Arc::new(Vec::new())),
            models_dir: None,
            backfill: true,
            fetch_models: false,
            models_changed: tokio::sync::watch::Sender::new(0),
            backfills: Mutex::new(HashMap::new()),
            fetch_tried: Mutex::new(HashSet::new()),
            model_jobs: Mutex::new(HashMap::new()),
            ranking: Models::default(),
            timeout,
            max_timeout: timeout.max(Duration::from_secs(60)),
            slow: None,
            profiling: telemetry::Profiling::default(),
            max_rows: None,
            max_rerank: None,
            max_groups: None,
            query_memory: None,
            engine: Engine::from_env(),
            max_statement_bytes: chloedb::query::parse::STATEMENT_BYTES,
            max_statement_elements: chloedb::query::parse::STATEMENT_ELEMENTS,
            max_queued_writes: usize::MAX,
            max_body: 256 << 20,
            otlp_retain: None,
            lease_ttl: Duration::from_secs(60),
            leases: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(1),
            scrolls: Mutex::new(HashMap::new()),
            max_scroll_rows: MAX_SCROLL_ROWS,
            stats_nonce: std::hash::BuildHasher::hash_one(&std::hash::RandomState::new(), (std::process::id(), Instant::now())),
            stale_stats: AtomicU64::new(0),
            admission: Arc::new(Semaphore::new(max_concurrent.max(1))),
            max_concurrent: max_concurrent.max(1),
            admission_wait: None,
            answers: None,
            embedding: Arc::new(Semaphore::new(cores)),
            named_models: Mutex::new(HashMap::new()),
            cores,
            running: Arc::new(AtomicUsize::new(0)),
            writes,
            writer_state,
            idempotency,
            synonyms: RwLock::new(None),
            gazetteers: std::sync::Mutex::new(HashMap::new()),
            synonyms_file: None,
            backup_dir: None,
            slices: None,
            slots: RwLock::new(None),
            slots_since: AtomicU64::new(0),
            slots_file: tokio::sync::Mutex::new(None),
            moving: Moving::default(),
            follows: feed::Follows::default(),
            replication: RwLock::new(Replication::None),
            promote_options: None,
            promoting: tokio::sync::Mutex::new(()),
            replication_file: None,
            failover_after: Duration::ZERO,
            scrubs: Default::default(),
            simulator: Default::default(),
        }
    }

    /// Its part in replication now.
    pub fn replication(&self) -> Replication {
        self.replication.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Sets its part in replication, before it serves.
    pub fn set_replication(&mut self, r: Replication) {
        *self.replication.get_mut().unwrap_or_else(|e| e.into_inner()) = r;
    }

    /// Its role in replication, as `Hello` says it.
    pub fn role(&self) -> pb::Role {
        match &self.replication() {
            Replication::None => pb::Role::Standalone,
            Replication::Primary(_) => pb::Role::Primary,
            Replication::Replica(_) => pb::Role::Replica,
        }
    }

    /// The newer term of a primary that replaced this one, as a primary
    /// (0: none).
    pub fn replaced_by(&self) -> u64 {
        match &self.replication() {
            Replication::Primary(p) => p.replaced_by(),
            _ => 0,
        }
    }

    /// Refuses a client's write on a replica (409), and one a primary may
    /// not apply now ([`Self::admit_write`]); answers who took it.
    fn writable(&self) -> Result<Admitted, ApiError> {
        self.takes_writes()?;
        self.admit_write()
    }

    /// Refuses a client's write on a replica, and on a shard of a cluster
    /// that follows another but from its follow process (409).
    fn takes_writes(&self) -> Result<(), ApiError> {
        self.is_writer()?;
        self.not_following(None, None)
    }

    /// Refuses a write on a replica (409): its writes are its primary's.
    pub(super) fn is_writer(&self) -> Result<(), ApiError> {
        match &self.replication() {
            Replication::Replica(_) => Err(conflict("this server is a replica: it applies its primary's writes and takes none from clients; write to the primary".into())),
            _ => Ok(()),
        }
    }

    /// Refuses a write a primary may not apply now ([`Primary::admit`]);
    /// answers who took it.
    fn admit_write(&self) -> Result<Admitted, ApiError> {
        match &self.replication() {
            Replication::Primary(p) => {
                let f = p.admit()?;
                Ok(Admitted(Some(self.term()), Some(f)))
            }
            _ => Ok(Admitted(None, None)),
        }
    }

    /// The term its database numbers writes under: a primary's (0 when
    /// none).
    fn term(&self) -> u64 {
        self.local().and_then(|db| db.storage().numbering()).map_or(0, |n| n.term)
    }

    /// Who takes a write now, unasked whether it may be applied: a
    /// statement that changes tables, and a retry of a write applied.
    fn admitted(&self) -> Admitted {
        match &self.replication() {
            Replication::Primary(_) => Admitted(Some(self.term()), None),
            _ => Admitted(None, None),
        }
    }

    /// Answers a committed write once the copies `--write-acks` asks for
    /// hold it, waiting up to `timeout` ([`Primary::acknowledged`]), on the
    /// primary that took it (`by`). One that stepped down since, as a
    /// primary does when it votes for another copy in a handover
    /// ([`Self::step_down`]), answers it in doubt (409), as it answers a
    /// write it applied when another replaced it before its copies held it
    /// ([`Primary::acknowledged`]): the new primary may not hold it, and
    /// this server, a replica now, waits for no copy.
    async fn replicated(&self, w: Written, timeout: Duration, by: Admitted) -> Result<Written, ApiError> {
        // Applied: a primary asked to hand its shard over waits no more
        // for it.
        let Admitted(by, applied) = by;
        drop(applied);
        let Some(seq) = w.seq_no else { return Ok(w) };
        match (&self.replication(), by) {
            // Taken by this primary, or on a server of its own, which was
            // made one since.
            (Replication::Primary(p), term) if term.is_none_or(|t| t == self.term()) => p.acknowledged(seq, Instant::now() + timeout).await?,
            (_, Some(term)) => {
                return Err(conflict(format!(
                    "the write is durable on this server as sequence number {seq}, but it stopped being the primary of term {term} before it was acknowledged, and the new primary may not hold it: write to the new primary"
                )))
            }
            _ => {}
        }
        Ok(w)
    }

    /// Applies a batch its primary shipped (`Replicate`), on a replica.
    pub async fn replicate(&self, r: pb::ReplicateRequest) -> Result<pb::ReplicateResponse, ApiError> {
        match &self.replication() {
            Replication::Replica(f) => {
                let before = self.catalog();
                let r = f.replicate(r).await;
                self.tables_shipped(&before);
                r
            }
            Replication::Primary(p) => p.shipped_to(&r.primary, r.term, r.config),
            Replication::None => Err(conflict("this server is not a replica (--replica): it takes no shipped transactions".into())),
        }
    }

    /// Installs a copy of its primary (`Restore`), on a replica.
    pub async fn restore(&self, chunks: tonic::Streaming<pb::RestoreChunk>) -> Result<pb::ReplicateResponse, ApiError> {
        match &self.replication() {
            Replication::Replica(f) => {
                let before = self.catalog();
                let r = f.restore(chunks).await;
                self.tables_shipped(&before);
                r
            }
            Replication::Primary(p) => p.copied_to(chunks).await,
            Replication::None => Err(conflict("this server is not a replica (--replica): it takes no copy of a primary".into())),
        }
    }

    /// Lists the models again when what a primary shipped, or its copy,
    /// changed the tables, which were `before`.
    fn tables_shipped(&self, before: &[Arc<Schema>]) {
        if self.catalog() != before {
            self.refresh_models();
        }
    }

    /// A replica's digests of its rows at the version its primary pinned
    /// (`Verify`).
    pub async fn verify_for(&self, r: pb::VerifyRequest) -> Result<pb::VerifyResponse, ApiError> {
        match &self.replication() {
            Replication::Replica(f) => f.verify(r).await,
            Replication::Primary(p) => {
                p.shipped_to(&r.primary, r.term, r.config)?;
                Err(conflict("this server is a primary: it has no rows of another primary's to compare".into()))
            }
            Replication::None => Err(conflict("this server is not a replica (--replica): it has no rows of a primary's to compare".into())),
        }
    }

    /// [`Self::verify_replicas`], every comparison made by this call: an
    /// answer holding one from before the call began (a replica that just
    /// joined, say) asks again, up to twice; one still older answers
    /// `compared: false` and why. Each says when it was made
    /// (`compared_ms`), and the answer when it was asked (`asked_ms`).
    pub async fn verify_fresh(&self, timeout: Duration) -> Result<Json, ApiError> {
        let start = telemetry::now_ms();
        let deadline = Instant::now() + timeout;
        let made = |r: &Json, now: u64| r["ms_ago"].as_u64().map(|a| now.saturating_sub(a));
        let mut j = Json::Null;
        for _ in 0..3 {
            j = self.verify_replicas(deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1))).await?;
            let now = telemetry::now_ms();
            let old = j["replicas"].as_array().into_iter().flatten().any(|r| r["in_sync"] == true && made(r, now).is_none_or(|at| at < start));
            if !old || Instant::now() >= deadline {
                break;
            }
        }
        let now = telemetry::now_ms();
        for r in j["replicas"].as_array_mut().into_iter().flatten() {
            let at = made(r, now);
            r["compared_ms"] = json!(at);
            match at {
                Some(at) if at < start => {
                    r["compared"] = json!(false);
                    r["why"] = json!(format!("no comparison was made in time: the last one is {} s old, from before this call", now.saturating_sub(at) / 1000));
                }
                None if r["compared"] != true => {
                    r["why"] = json!(if r["in_sync"] == true { "no comparison was made in time" } else { "out of sync: a replica is compared once it is in sync" });
                }
                _ => {}
            }
        }
        j["asked_ms"] = json!(start);
        Ok(j)
    }

    /// Compares each replica's rows with this primary's now
    /// ([`Primary::verify`]), waiting up to `timeout`.
    pub async fn verify_replicas(&self, timeout: Duration) -> Result<Json, ApiError> {
        match &self.replication() {
            Replication::Primary(p) => Ok(p.verify(Instant::now() + timeout).await),
            _ => Err(conflict("this server is not a primary (--replicas): it has no replicas to compare".into())),
        }
    }

    /// The shard's copies, as this replica knows them: its `--replicas`,
    /// and the servers its cluster map names for its shard (which only the
    /// cluster's token sets). The only servers a promotion asked for with
    /// a key calls, and so sends the cluster's token to.
    pub fn known_copies(&self) -> Vec<String> {
        let mut known = self.promote_options.as_ref().map(|o| o.replicas.clone()).unwrap_or_default();
        if let Some((m, shard)) = self.slots() {
            for url in m.replicas().get(shard).into_iter().flatten() {
                if !known.contains(url) {
                    known.push(url.clone());
                }
            }
        }
        known
    }

    /// Makes this replica the primary of a new term, shipping to
    /// `replicas` under `acks` ([`Follow::promote`]), once a majority of
    /// its shard's copies voted for it: the servers its cluster map names
    /// for its shard, or else `replicas`. With no `replicas`, it ships to
    /// the shard's other copies, or without a cluster map to its
    /// `--replicas`. With `force`, it asks for no votes, and is promoted
    /// even when a copy holds transactions it does not. Promotion calls
    /// each copy with the cluster's token, so a key's promotion may name
    /// only [`Self::known_copies`]; the token's holder, who could send the
    /// token anywhere anyway, may name others.
    pub async fn promote(&self, who: &Principal, replicas: Vec<String>, acks: Acks, force: bool) -> Result<Json, ApiError> {
        who.may(Scope::Admin, "a promotion")?;
        let known = self.known_copies();
        let unknown: Vec<&String> = replicas.iter().filter(|u| !known.contains(u)).collect();
        if !who.root && !unknown.is_empty() {
            let known = match known.is_empty() {
                true => "none: start the replica with --replicas naming them, or add it to a cluster".to_string(),
                false => known.join(", "),
            };
            return Err(ApiError {
                code: None, status: 403,
                message: format!(
                    "{} {} not among this shard's copies, which promotion calls with the cluster's token; known copies: {known}",
                    unknown.iter().map(|u| u.as_str()).collect::<Vec<_>>().join(", "),
                    if unknown.len() == 1 { "is" } else { "are" }
                ),
            });
        }
        let _one = self.promoting.lock().await;
        let Replication::Replica(f) = self.replication() else {
            return Err(conflict("this server is not a replica (--replica): only a replica is promoted".into()));
        };
        let voters = self.slots().and_then(|(m, shard)| m.replicas().get(shard).cloned()).filter(|v| v.len() > 1);
        // None named, and no cluster map: the copies it was started with.
        let replicas = match (replicas.is_empty(), &voters) {
            (true, None) => self.promote_options.as_ref().map(|o| o.replicas.clone()).unwrap_or_default(),
            _ => replicas,
        };
        keys::note(format!(
            "promote, shipping to {}",
            match (replicas.is_empty(), &voters) {
                (false, _) => replicas.join(", "),
                (true, Some(_)) => "the shard's other copies".to_string(),
                (true, None) => "no replica".to_string(),
            }
        ));
        let opts = PrimaryOptions { replicas, acks, ..self.primary_options() };
        let (p, answer) = f.promote(opts, voters, force).await?;
        *self.replication.write().unwrap_or_else(|e| e.into_inner()) = Replication::Primary(p);
        // A primary fills in what its rows lack.
        self.models_changed.send_modify(|n| *n += 1);
        Ok(answer)
    }

    /// How this server ships once promoted ([`Self::promote_options`]),
    /// with the lease on reads automatic failover needs.
    fn primary_options(&self) -> PrimaryOptions {
        failover::tuned(self.promote_options.clone().unwrap_or_default(), self.failover_after)
    }

    /// Sets this server's part in replication while it serves
    /// (`Configure`), kept beside its database: `Primary` ships to
    /// `replicas` under `acks` (as now when `None`, else all) from now on,
    /// starting to ship on a standalone server; `Replica` makes a
    /// standalone server wait for a primary, whose copy replaces its rows
    /// (refused while it holds rows, unless `force`), or a primary another
    /// replaced (any primary with `force`) follow the new one;
    /// `Standalone` ends replication (a replica's only with `force`).
    /// A replica is made primary by promotion ([`Self::promote`]), not
    /// here.
    pub async fn configure(&self, role: pb::Role, replicas: Vec<String>, acks: Option<Acks>, force: bool) -> Result<pb::ConfigureResponse, ApiError> {
        self.configure_as(role, replicas, acks, force, false).await
    }

    /// [`Self::configure`]. A serving primary takes other replicas only as
    /// [`crate::cores::shard::Primary::may_reconfigure`] allows: one copy
    /// at a time, while a majority of its copies is with it, waiting up to
    /// [`RECONFIGURE_WAIT`] for the change before to reach them; or with
    /// `accept_data_loss`, which it logs.
    pub async fn configure_as(&self, role: pb::Role, replicas: Vec<String>, acks: Option<Acks>, force: bool, accept_data_loss: bool) -> Result<pb::ConfigureResponse, ApiError> {
        let _one = self.promoting.lock().await;
        let db = self.local().cloned().ok_or_else(|| ApiError::bad("a coordinator has no part in replication: configure its shards' servers"))?;
        let file = self.replication_file.clone();
        let now = self.replication();
        let next = match (role, &now) {
            (pb::Role::Primary, Replication::Primary(p)) => {
                let acks = acks.unwrap_or(p.acks());
                match p.replicas() == replicas.as_slice() && p.acks() == acks {
                    true => None,
                    false => {
                        let until = Instant::now() + RECONFIGURE_WAIT;
                        while let Err(e) = p.may_reconfigure(&replicas, accept_data_loss) {
                            if Instant::now() >= until || e.status != 503 {
                                tracing::warn!(error = %e.message, replicas = %replicas.join(", "), "refused other replicas");
                                return Err(e);
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Some(Replication::Primary(p.reconfigure(replicas, acks)?))
                    }
                }
            }
            (pb::Role::Primary, Replication::None) => {
                if db.storage().numbering().is_none() {
                    return Err(ApiError::bad("a primary numbers its writes, and this server's database does not: start it with --seq-no"));
                }
                let opts = PrimaryOptions { replicas, acks: acks.unwrap_or_default(), joining: true, ..self.primary_options() };
                let p = Primary::start(db.clone(), file.as_deref(), opts).map_err(|e| conflict(e))?;
                tracing::info!(replicas = %p.replicas().join(", "), write_acks = p.acks().name(), "configured: the primary of its shard");
                Some(Replication::Primary(p))
            }
            (pb::Role::Primary, Replication::Replica(_)) => {
                return Err(conflict("this server is a replica: promote it to make it the primary of a new term (POST /v1/admin/promote)".into()));
            }
            (pb::Role::Replica, Replication::Replica(_)) => None,
            (pb::Role::Replica, Replication::Primary(p)) if p.replaced_by() == 0 && !force => {
                return Err(conflict("this server is its shard's primary, and no primary replaced it: promote another copy first (or pass force)".into()));
            }
            (pb::Role::Replica, _) => {
                if db.storage().numbering().is_none() {
                    return Err(ApiError::bad("a replica numbers its writes as its primary does: start it with --seq-no"));
                }
                let rows = db.snapshot().n_live_docs();
                if matches!(now, Replication::None) && rows > 0 && !force {
                    return Err(conflict(format!("this server holds {rows} rows, which its primary's copy would replace: pass force to drop them")));
                }
                let f = Follow::open(db.clone(), file.as_deref()).map_err(ApiError::bad)?;
                tracing::info!("configured: a replica, which applies what its primary ships");
                Some(Replication::Replica(Arc::new(f)))
            }
            (pb::Role::Standalone, Replication::None) => None,
            (pb::Role::Standalone, Replication::Replica(_)) if !force => {
                return Err(conflict("this server is a replica: on its own it would take writes its primary does not hold; pass force".into()));
            }
            (pb::Role::Standalone, _) => {
                crate::replication::forget_role(file.as_deref()).map_err(|e| ApiError::internal(format!("keep the replication role: {e}")))?;
                tracing::info!("configured: standalone, replicating no more");
                Some(Replication::None)
            }
        };
        if let Some(r) = next {
            *self.replication.write().unwrap_or_else(|e| e.into_inner()) = r;
        }
        Ok(self.configured())
    }

    /// Its part in replication, as `Configure` answers it.
    pub fn configured(&self) -> pb::ConfigureResponse {
        let n = self.local().and_then(|db| db.storage().numbering()).unwrap_or_default();
        let (replicas, write_acks) = match &self.replication() {
            Replication::Primary(p) => (p.replicas().to_vec(), p.acks().name().to_string()),
            _ => (Vec::new(), String::new()),
        };
        pb::ConfigureResponse { role: self.role() as i32, term: n.term, seq_no: n.seq, replicas, write_acks }
    }

    /// Keeps its part in replication in `path` (`<db>.replication`), for
    /// [`Self::configure`].
    pub fn keep_replication_in(&mut self, path: PathBuf) {
        self.replication_file = Some(path);
    }

    /// Keeps the slot map a coordinator gives this shard in `path`, and
    /// reads the one kept there, if any. Without a file, the map lasts
    /// until the server stops.
    pub fn keep_slots_in(&mut self, path: PathBuf) -> Result<(), String> {
        if path.exists() {
            let j: Json = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string())).map_err(|e| format!("{}: {e}", path.display()))?;
            let map = SlotMap::from_json(&j["map"]).map_err(|e| format!("{}: {e}", path.display()))?;
            let shard = j["shard"].as_u64().map(|s| s as usize).filter(|&s| s < map.shards()).ok_or_else(|| format!("{}: no shard of the map", path.display()))?;
            *self.slots_since.get_mut() = j["since"].as_u64().unwrap_or(map.epoch).min(map.epoch);
            self.hide_residue(&map, shard);
            *self.slots.get_mut().unwrap_or_else(|e| e.into_inner()) = Some((map, shard));
        }
        *self.slots_file.get_mut() = Some(path);
        Ok(())
    }

    /// The epoch of this shard's slot map; 0 without one.
    pub fn slot_epoch(&self) -> u64 {
        self.slots.read().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |(m, _)| m.epoch)
    }

    /// The coordinator this server is, for the calls that change the
    /// cluster; a database refuses them (400).
    /// The cores this server may use.
    pub fn cores(&self) -> usize {
        self.cores
    }

    pub fn coordinator(&self) -> Result<&Arc<Coordinator>, ApiError> {
        match &self.backend {
            Backend::Cluster(c) => Ok(c),
            Backend::Local(_) => Err(ApiError::bad("this server is a database, not a coordinator (--shards): change the cluster through its coordinator")),
        }
    }

    /// This shard's slot map and its place in it, if a coordinator gave one.
    pub fn slots(&self) -> Option<(SlotMap, usize)> {
        self.slots.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Takes `map`, with this server as its shard `shard`, from a
    /// coordinator, as [`core::take`] decides: a map of an older epoch than
    /// this shard's is refused (409), as is another map of the same epoch.
    /// Every row here must be in the shard's slots under a map that changes
    /// them, but for the next step of a move, whose rows the coordinator
    /// moved: a shard loaded by another map, or by the hash before slots,
    /// refuses the map (409) until it is loaded again. A map that leaves
    /// the shard its slots (another shard's, or servers, changed) keeps
    /// serving coordinators that route by the one before. A map that gives
    /// away the slots moving from here is taken only while writes to them
    /// were held since their rows were last exported ([`core::flip_ok`]).
    /// Returns the epoch now held.
    pub async fn set_slots(&self, map: SlotMap, shard: usize) -> Result<u64, ApiError> {
        if shard >= map.shards() {
            return Err(ApiError::bad(format!("shard {shard} of a slot map of {} shards", map.shards())));
        }
        let db = self.local().cloned().ok_or_else(|| ApiError::bad("a coordinator holds no slots"))?;
        let file = self.slots_file.lock().await;
        // Held until the map is set, so that the hold cannot run out
        // between the check and the change.
        let frozen = self.moving.frozen.lock().await;
        let held = self.slots();
        // Only a server that takes clients' writes may have taken some to
        // the moving slots that no export copied: a replica's come from its
        // primary, and a primary another replaced takes none.
        let writer = match &self.replication() {
            Replication::Replica(_) => false,
            Replication::Primary(p) => p.replaced_by() == 0,
            Replication::None => true,
        };
        let exporting = held.as_ref().and_then(|(m, s)| core::exporting(m, *s)).cloned();
        let covered = !writer || self.moving.covers(exporting.as_ref(), frozen.as_ref().map(|f| f.since));
        core::flip_ok(held.as_ref().map(|(m, s)| (m, *s)), &map, shard, covered)?;
        let (since, check_rows) = match core::take(held.as_ref().map(|(m, s)| (m, *s)), self.slots_since.load(Ordering::Relaxed), &map, shard)? {
            core::Take::Held => return Ok(map.epoch),
            core::Take::Keep { since, check_rows } => (since, check_rows),
        };
        if check_rows {
            let (d, m) = (db.clone(), map.clone());
            let elsewhere = tokio::task::spawn_blocking(move || rows_elsewhere(&d, &m, shard)).await.map_err(ApiError::internal)??;
            if let Some((n, key)) = elsewhere {
                let other = map.shard_of(&key);
                return Err(conflict(format!(
                    "{n} rows here have keys in other shards' slots under slot map epoch {} (key {} is in slot {}, which shard {other} holds, not {shard}): load the collection again through the coordinator (or with --demo-shard), then connect",
                    map.epoch,
                    js::to_json(&key),
                    slots::slot(&key)
                )));
            }
        }
        // The slots that change hands here are marked in the log before
        // the map lets a write reach them, so that a cluster following this
        // one orders the writes on either side ([`feed`]), and the stream
        // offsets they arrive with are kept for them ([`arrivals`]).
        if let Some((old, s)) = held.as_ref().filter(|(_, s)| writer && *s == shard) {
            let marks = match db.storage().numbering() {
                Some(_) => feed::flip_marks(old, &map, *s),
                None => Vec::new(),
            };
            if !marks.is_empty() || !old.same_slots(&map, shard) {
                let (d, old, new, deadline) = (db.clone(), old.clone(), map.clone(), Instant::now() + self.timeout);
                tokio::task::spawn_blocking(move || wait_for_writer(deadline, || write_flip(&d, &marks, &old, &new, shard))).await.map_err(ApiError::internal)??;
            }
        }
        if let Some(path) = &*file {
            save_slots(path, &map, shard, since).map_err(|e| ApiError::internal(format!("keep the slot map in {}: {e}", path.display())))?;
        }
        let epoch = map.epoch;
        // The rows a read sees change with the map: a read takes its
        // version under this lock ([`Self::latest`]).
        let mut held = self.slots.write().unwrap_or_else(|e| e.into_inner());
        self.slots_since.store(since, Ordering::Relaxed);
        self.hide_residue(&map, shard);
        *held = Some((map, shard));
        drop(frozen);
        Ok(epoch)
    }

    /// Checks a request's slot map epoch against this shard's: 0 (a
    /// request not from a coordinator with a map) passes, as does an
    /// older epoch under which this shard held the slots it holds;
    /// another older epoch is refused (409), as the coordinator routes by
    /// a map the shards left; a newer one, or one this shard lacks, is
    /// refused as unavailable (503), so that the coordinator asks again
    /// and sends its map.
    pub fn check_epoch(&self, epoch: u64) -> Result<(), ApiError> {
        let _ = ROUTED.try_with(|r| r.set(epoch));
        let held = self.slots.read().unwrap_or_else(|e| e.into_inner());
        self.epoch_ok(held.as_ref().map_or(0, |(m, _)| m.epoch), epoch)
    }

    /// [`Self::check_epoch`] against a map of epoch `held` (0: none)
    /// ([`core::epoch_ok`]).
    fn epoch_ok(&self, held: u64, epoch: u64) -> Result<(), ApiError> {
        core::epoch_ok(held, self.slots_since.load(Ordering::Relaxed), epoch)
    }


    /// Refuses (400) to write a row placed in a slot another shard holds,
    /// when this shard has a slot map: by its key, or by its table's
    /// shard_by value ([`op_place`]), each named by `what` (`key`, or the
    /// shard_by field's name). Rows of moving slots pass as `moved` says
    /// ([`core::misplaced`]): copied by the coordinator moving them to the
    /// shard they go to while the move copies, or deleted by one of the
    /// move's two shards. A request routed by an older map that this one
    /// changed since it was checked is refused for its coordinator to
    /// route it again (409).
    fn check_keys<'a>(&self, keys: impl IntoIterator<Item = (&'a str, &'a Value)>, moved: Moved) -> Result<(), ApiError> {
        let held = self.slots.read().unwrap_or_else(|e| e.into_inner());
        let Some((map, shard)) = &*held else { return Ok(()) };
        for (what, key) in keys {
            let s = slots::slot(key);
            if let Some(m) = core::misplaced(map, *shard, s, moved, routed()) {
                return Err(m.error(map, *shard, s, what, &js::to_json(key).to_string()));
            }
        }
        Ok(())
    }

    /// Opens the model of each embedded VECTOR field, of every table, from
    /// `dir` ([`chloedb::embed::open`]); a local database computes the
    /// vectors its writes leave out with them, and opens those of tables
    /// created later ([`Db::open_embedders`]).
    pub fn open_models(&mut self, dir: &std::path::Path) {
        self.models_dir = Some(dir.to_path_buf());
        if let Backend::Local(db) = &self.backend {
            db.open_embedders(dir);
        }
        self.refresh_models();
    }

    /// `plan` with its ranking models opened ([`chloedb::query::model`]):
    /// a database's from its models directory, and a coordinator's from
    /// its own, since it scores the shards' rows with them too. The plan
    /// then names each model's hash, which every shard checks against its
    /// own file. A `shard` leaves cross-encoders to its coordinator, which
    /// refuses a phase that would score more rows with one than
    /// `--max-rerank` lets it ([`Service::max_rerank`]), as a database
    /// does.
    pub fn open_ranking_models(&self, plan: Arc<Plan>, shard: bool) -> Result<Arc<Plan>, ApiError> {
        if !plan.root.has_models() {
            return Ok(plan);
        }
        if let Some(max) = self.max_rerank.filter(|_| !shard) {
            let mut over = None;
            plan.root.each_search(&mut |s| {
                for p in &s.phases {
                    let mut cross = false;
                    p.expr.visit(&mut |x| cross |= matches!(x, chloedb::query::ir::Expr::CrossEncoder { .. }));
                    if cross && p.count > max as u64 {
                        over.get_or_insert(p.count);
                    }
                }
            });
            if let Some(n) = over {
                return Err(ApiError::bad(format!("a cross-encoder scores at most {max} rows of a query here (--max-rerank), not {n}: rerank fewer rows by it")));
            }
        }
        let mut p = (*plan).clone();
        match &self.backend {
            Backend::Local(db) => db.open_ranking_models(&mut p, shard)?,
            Backend::Cluster(_) => self.ranking.open(&self.models_dir.clone().unwrap_or_else(chloedb::embed::models_dir), &mut p, true)?,
        }
        Ok(Arc::new(p))
    }

    /// Where this server's models are: `--models`, else the default.
    pub fn models_dir(&self) -> std::path::PathBuf {
        self.models_dir.clone().unwrap_or_else(chloedb::embed::models_dir)
    }

    /// The embedded fields' models ([`Service::open_models`]).
    pub fn models(&self) -> Arc<Vec<FieldModel>> {
        self.models.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Lists the model of each embedded field of every table now, keeping
    /// those already listed: after `CREATE TABLE`, and when a coordinator
    /// learns of new tables. Nothing before [`Service::open_models`].
    fn refresh_models(&self) {
        self.list_models(false)
    }

    /// [`Self::refresh_models`], opening again the models that did not open
    /// (after a fetch).
    fn reopen_models(&self) {
        self.list_models(true)
    }

    fn list_models(&self, reopen: bool) {
        let Some(dir) = &self.models_dir else { return };
        let old = self.models();
        let mut list = Vec::new();
        for t in self.catalog() {
            for f in &t.fields {
                let Some(spec) = f.embed.clone() else { continue };
                if let Some(m) = old.iter().find(|m| m.table.eq_ignore_ascii_case(&t.table) && m.field == f.name && m.spec == spec).filter(|m| !reopen || m.model.is_ok()) {
                    list.push(m.clone());
                    continue;
                }
                let dim = match f.ty {
                    chloedb::schema::FieldType::Vector { dim, .. } => Some(dim),
                    _ => None,
                };
                let model = match &self.backend {
                    Backend::Local(db) => match db.embedder_in(&t.table, &f.name) {
                        Some(m) => Ok(m),
                        None => chloedb::embed::open_for(&spec.model, dim, dir).and_then(|m| db.set_embedder_in(&t.table, &f.name, m.clone()).map(|_| m)),
                    },
                    Backend::Cluster(_) => chloedb::embed::open_for(&spec.model, dim, dir),
                };
                list.push(FieldModel { table: t.table.clone(), field: f.name.clone(), spec, model: model.map_err(|e| e.to_string()) });
            }
        }
        // A field with a model it did not have: its rows may lack vectors.
        let opened = |l: &[FieldModel]| l.iter().filter(|m| m.model.is_ok()).map(|m| (m.table.to_lowercase(), m.field.clone(), m.spec.clone())).collect::<Vec<_>>();
        let gained = opened(&list).iter().any(|m| !opened(&old).contains(m));
        // A field new here whose model did not open (a table created or
        // altered since): its model may be one to fetch.
        let same = |a: &FieldModel, b: &FieldModel| a.table.eq_ignore_ascii_case(&b.table) && a.field == b.field && a.spec == b.spec;
        let wants = list.iter().any(|m| m.model.is_err() && !old.iter().any(|o| same(o, m)));
        *self.models.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(list);
        if gained || wants {
            self.models_changed.send_modify(|n| *n += 1);
        }
    }

    /// Keeps the embedded fields' models, for the server's life: fetches
    /// those of [`chloedb::embed::KNOWN_MODELS`] a field names that the
    /// models directory lacks ([`Self::fetch_models`]), then fills in the
    /// vectors of rows stored without one while their field had no model
    /// open, on a database that takes writes ([`Self::backfill`]). It runs
    /// at start and whenever a field gains a model (a table created or
    /// altered, a model fetched) or a replica becomes primary; a backfill
    /// a model failed is tried again five minutes later.
    pub async fn keep_models(self: Arc<Self>) {
        let mut changed = self.models_changed.subscribe();
        loop {
            // A change from here on is another pass.
            changed.borrow_and_update();
            if self.fetch_models {
                self.fetch_known_models().await;
            }
            let again = self.backfill && self.backfill_all().await;
            match again {
                true => drop(tokio::time::timeout(Duration::from_secs(300), changed.changed()).await),
                false => drop(changed.changed().await),
            }
        }
    }

    /// Fetches the models [`Self::keep_models`] fetches, each once, and
    /// opens them.
    async fn fetch_known_models(&self) {
        let Some(dir) = self.models_dir.clone() else { return };
        let mut fetched = false;
        for m in self.models().iter().filter(|m| m.model.is_err()) {
            let Some(name) = chloedb::embed::to_fetch(&m.spec.model, &dir) else { continue };
            if !self.fetch_tried.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string()) {
                continue;
            }
            tracing::info!(table = %m.table, field = %m.field, model = name, "fetching the model of an embedded field");
            match crate::server::fetch_model(name, &dir).await {
                Ok(_) => fetched = true,
                Err(e) => tracing::warn!(model = name, error = %e, "could not fetch the model of an embedded field: fetch it with chloedb-server fetch-model, or start with --no-fetch-models"),
            }
        }
        if fetched {
            self.reopen_models();
        }
    }

    /// The fetches and conversions of models asked for over HTTP.
    pub fn model_jobs(&self) -> HashMap<String, ModelJob> {
        self.model_jobs.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Starts fetching model `name` of [`chloedb::embed::KNOWN_MODELS`]
    /// into the models directory (`convert: None`), or writing static model
    /// `name`'s table again in `I8` or `F16` as a model of its own,
    /// `<name>-int8` or `<name>-f16` (`convert: Some(type)`), and answers
    /// at once with the name of the model it makes; [`Self::model_jobs`]
    /// says how it went. Then the fields that name the model open it, and
    /// fill in the vectors their rows lack. Names only: never a path, nor
    /// a repository the server does not know.
    pub fn start_model_job(self: &Arc<Self>, name: &str, convert: Option<&str>) -> Result<String, ApiError> {
        let dir = self.models_dir();
        let (target, from) = match convert {
            None => {
                let &(known, _, _) = chloedb::embed::KNOWN_MODELS.iter().find(|m| m.0.eq_ignore_ascii_case(name)).ok_or_else(|| {
                    let names: Vec<&str> = chloedb::embed::KNOWN_MODELS.iter().map(|m| m.0).collect();
                    ApiError::bad(format!("{name} is not a model this server fetches: one of {}", names.join(", ")))
                })?;
                // N-D12: a server told not to download (an air-gapped
                // install, a network policy) makes no outbound request when
                // an admin clicks Fetch either.
                if !self.fetch_models {
                    return Err(ApiError { code: None, status: 409, message: format!("this server does not download models (--no-fetch-models): fetch {known} with chloedb-server fetch-model {known} --models DIR, or restart it without the flag") });
                }
                (known.to_string(), None)
            }
            Some(elem) => {
                if let Some(why) = chloedb::query::embed_sql::refuse_model(name, &dir) {
                    return Err(ApiError::bad(format!("model {name}: {why}")));
                }
                let suffix = match elem.to_ascii_uppercase().as_str() {
                    "I8" | "INT8" => "int8",
                    "F16" | "FLOAT16" => "f16",
                    other => return Err(ApiError::bad(format!("a static model converts to I8 or F16, not {other}"))),
                };
                let d = chloedb::embed::describe(name, &dir);
                if d.technique != "static" || !dir.join(name).join("model.safetensors").exists() {
                    return Err(ApiError::bad(format!("model {name} is not a static model here: only a Model2Vec table converts (fetch it first)")));
                }
                if d.precision.as_deref().is_some_and(|p| p == "I8" || (p == "F16" && suffix == "f16")) {
                    return Err(ApiError::bad(format!("model {name}'s table is {} already", d.precision.unwrap_or_default())));
                }
                (format!("{name}-{suffix}"), Some((name.to_string(), elem.to_ascii_uppercase().replace("INT8", "I8").replace("FLOAT16", "F16"))))
            }
        };
        if from.is_some() && dir.join(&target).join("model.safetensors").exists() {
            return Err(ApiError { code: None, status: 409, message: format!("model {target} is here already") });
        }
        {
            let mut jobs = self.model_jobs.lock().unwrap_or_else(|e| e.into_inner());
            if jobs.get(&target.to_lowercase()).is_some_and(|j| j.state == "running") {
                return Err(ApiError { code: None, status: 409, message: format!("model {target} is being made already") });
            }
            let job = ModelJob { action: if from.is_some() { "convert" } else { "fetch" }, from: from.as_ref().map(|f| f.0.clone()), state: "running", error: None, started_ms: crate::telemetry::now_ms(), finished_ms: None };
            jobs.insert(target.to_lowercase(), job);
        }
        let svc = self.clone();
        let t = target.clone();
        tokio::spawn(async move {
            let done = match from {
                None => crate::server::fetch_model(&t, &dir).await.map(|_| ()),
                Some((src, elem)) => {
                    let (a, b) = (dir.join(&src), dir.join(&t));
                    tokio::task::spawn_blocking(move || chloedb::embed::quantize_static(&a, &b, &elem).map(|_| ()).map_err(|e| e.to_string())).await.unwrap_or_else(|e| Err(e.to_string()))
                }
            };
            match &done {
                Ok(()) => tracing::info!(model = %t, "made a model asked for over HTTP"),
                Err(e) => tracing::warn!(model = %t, error = %e, "could not make a model asked for over HTTP"),
            }
            if let Some(j) = svc.model_jobs.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&t.to_lowercase()) {
                j.state = if done.is_ok() { "done" } else { "failed" };
                j.error = done.as_ref().err().cloned();
                j.finished_ms = Some(crate::telemetry::now_ms());
            }
            if done.is_ok() {
                svc.reopen_models();
            }
        });
        Ok(target)
    }

    /// Fills in the vectors each embedded field's rows lack
    /// ([`Self::backfill_field`]); whether a model failed, for another try.
    async fn backfill_all(self: &Arc<Self>) -> bool {
        let Some(db) = self.local().cloned() else { return false };
        let mut again = false;
        for m in self.models().iter().filter(|m| m.model.is_ok()) {
            // A replica's rows come from its primary.
            if self.takes_writes().is_err() {
                return false;
            }
            again |= self.backfill_field(&db, &m.table, &m.field).await;
        }
        again
    }

    /// Fills in the vectors of `table.field` that rows lack
    /// ([`Db::missing_vectors`]), [`BACKFILL_BATCH`] rows a write, through
    /// the writer as any statement goes; whether the model failed, for
    /// another try later.
    async fn backfill_field(self: &Arc<Self>, db: &Arc<Db>, table: &str, field: &str) -> bool {
        let id = (table.to_lowercase(), field.to_string());
        let set = |b: Backfill| {
            self.backfills.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), b);
        };
        let (d, t, f) = (db.clone(), table.to_string(), field.to_string());
        let keys = match tokio::task::spawn_blocking(move || d.missing_vectors(&t, &f)).await {
            Ok(Ok(keys)) => keys,
            Ok(Err(e)) => {
                tracing::warn!(table, field, error = %e, "cannot fill in the vectors rows lack");
                set(Backfill { state: "failed", missing: 0, embedded: 0, error: Some(e.to_string()) });
                return false;
            }
            Err(_) => return false,
        };
        let missing = keys.len() as u64;
        let before = self.backfills.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
        // Rows without text get no vector: nothing to do again.
        if before.as_ref().is_some_and(|b| b.state == "done" && b.missing - b.embedded == missing) || (before.is_none() && missing == 0) {
            return false;
        }
        tracing::info!(table, field, rows = missing, "filling in the vectors of rows stored without one");
        let t0 = Instant::now();
        let mut b = Backfill { state: "running", missing, embedded: 0, error: None };
        set(b.clone());
        for chunk in keys.chunks(BACKFILL_BATCH) {
            let mut busy = 0;
            let written = loop {
                let (t, f, chunk) = (table.to_string(), field.to_string(), chunk.to_vec());
                let guard = self.statement_guard(0, Arc::new(Mutex::new(None)));
                let prepare = move |db: &Db| db.prepare_backfill(&t, &f, chunk).map(|p| Some(p.guarded(guard)));
                match self.write_local(db, prepare, 0, None).await {
                    // The queue is full: clients' writes come first.
                    Err(e) if e.status == 429 && busy < 600 => {
                        busy += 1;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    r => break r,
                }
            };
            match written {
                Ok(w) => {
                    b.embedded += w.rows;
                    set(b.clone());
                }
                Err(e) => {
                    tracing::warn!(table, field, embedded = b.embedded, error = %e.message, "filling in vectors stopped; tried again in five minutes");
                    set(Backfill { state: "failed", error: Some(e.message.clone()), ..b });
                    return e.status >= 500 || e.status == 429;
                }
            }
        }
        self.metrics.embedded("backfill", b.embedded, t0.elapsed().as_secs_f64());
        tracing::info!(table, field, embedded = b.embedded, without_text = missing - b.embedded, seconds = t0.elapsed().as_secs_f64(), "filled in the vectors of rows stored without one");
        set(Backfill { state: "done", ..b });
        false
    }

    /// The backfill of `table.field` ([`Self::keep_models`]), if it had
    /// rows to fill in.
    pub fn backfill_of(&self, table: &str, field: &str) -> Option<Backfill> {
        self.backfills.lock().unwrap_or_else(|e| e.into_inner()).get(&(table.to_lowercase(), field.to_string())).cloned()
    }

    /// Whether this server embeds any field's text.
    fn embeds(&self) -> bool {
        self.models().iter().any(|m| m.model.is_ok())
    }

    /// The model of embedded field `field` of `table`, or of the first
    /// embedded field that has one: of `table` when it is given, else of
    /// the first table that has such a field, in the database's order.
    pub fn model(&self, table: Option<&str>, field: Option<&str>) -> Result<(String, String, Arc<dyn Embedder>), ApiError> {
        let mut models = self.models();
        // A table created since the models were listed.
        if table.is_some_and(|t| !models.iter().any(|m| m.table.eq_ignore_ascii_case(t))) {
            self.refresh_models();
            models = self.models();
        }
        let of_table = |m: &&FieldModel| table.is_none_or(|t| m.table.eq_ignore_ascii_case(t));
        let named = match table {
            Some(t) => format!("{t}.{}", field.unwrap_or("")),
            None => field.unwrap_or("").to_string(),
        };
        let m = match field {
            Some(f) => models.iter().filter(of_table).find(|m| m.field.eq_ignore_ascii_case(f)).ok_or_else(|| ApiError::bad(format!("{named} is not an embedded VECTOR field; pass a vector")))?,
            None => models.iter().filter(of_table).find(|m| m.model.is_ok()).ok_or_else(|| ApiError::bad("no table has a field this server can embed text for; pass a vector"))?,
        };
        match &m.model {
            Ok(e) => Ok((m.table.clone(), m.field.clone(), e.clone())),
            Err(why) => Err(ApiError::bad(format!("{}'s model {} is not available here ({why}); pass a vector", m.field, m.spec.model))),
        }
    }

    /// Sets how a local database's writes resolve the MEDIA values they
    /// give (`--media-fetch`, `--media-max-bytes`, `--media-store`,
    /// [`chloedb::db::Db::set_media_resolver`]). A coordinator refuses: its
    /// shards fetch for the writes they take.
    pub fn set_media(&self, r: chloedb::media::Resolver) -> Result<(), ApiError> {
        match &self.backend {
            Backend::Local(db) => {
                db.set_media_resolver(r);
                Ok(())
            }
            Backend::Cluster(_) => Err(ApiError::bad("--media-fetch, --media-max-bytes and --media-store go to each shard server, which fetches the files of the writes it takes")),
        }
    }

    /// Whether a table has a MEDIA field, whose values writes resolve
    /// before the writer ([`Service::embed_ops`]).
    fn has_media(&self) -> bool {
        self.catalog().iter().any(|t| t.fields.iter().any(|f| f.media.is_some()))
    }

    /// Does the work the documents of `ops` leave to the server before
    /// they queue for the writer: resolves their MEDIA values (fetching
    /// files, [`chloedb::db::Db::resolve_media_in`]), makes the derived text
    /// and computes the embedded vectors they leave out, so that models run
    /// on many cores rather than in the writer thread, and the rows logged
    /// (and shipped to replicas) hold the results.
    fn embed_ops(&self, db: &Db, ops: &mut [Json]) -> Result<(), ApiError> {
        let models = self.models();
        let writes_doc = |op: &Json| matches!(op.get("op").and_then(|o| o.as_str()), Some("insert" | "insert_if_absent" | "upsert" | "upsert_if_newer"));
        let now = chloedb::timestamp::now();
        let t0 = Instant::now();
        let mut n = 0;
        let first = chloedb::db::default_table(&db.catalog()).map_or_else(|| db.schema().table.clone(), |t| t.table.clone());
        for schema in db.catalog() {
            let wanted: Vec<usize> = models.iter().filter(|m| m.model.is_ok() && m.table.eq_ignore_ascii_case(&schema.table)).filter_map(|m| schema.field_index(&m.field)).collect();
            let media: Vec<usize> = (0..schema.fields.len()).filter(|&f| schema.fields[f].media.is_some()).collect();
            let derived: Vec<usize> = (0..schema.fields.len()).filter(|&f| schema.fields[f].derive.is_some()).collect();
            if wanted.is_empty() && media.is_empty() {
                continue;
            }
            let mut at = Vec::new();
            let mut docs = Vec::new();
            for (i, op) in ops.iter().enumerate() {
                if writes_doc(op) && op.get("doc").is_some_and(|d| d.is_object()) && op_table(op, &first)?.eq_ignore_ascii_case(&schema.table) {
                    docs.push(js::document(&schema, &op["doc"], now)?);
                    at.push(i);
                }
            }
            let before: Vec<Vec<bool>> = docs.iter().map(|d| wanted.iter().map(|&f| matches!(d[f], Value::Null)).collect()).collect();
            let untold: Vec<Vec<bool>> = docs.iter().map(|d| derived.iter().map(|&f| matches!(d[f], Value::Null)).collect()).collect();
            db.fill_in(&schema.table, &mut docs)?;
            n += before.iter().flatten().filter(|&&m| m).count() as u64;
            for (((i, d), missing), untold) in at.into_iter().zip(&docs).zip(before).zip(untold) {
                for (&f, was_missing) in wanted.iter().zip(missing) {
                    if let (true, Value::Vector(v)) = (was_missing, &d[f]) {
                        ops[i]["doc"][schema.fields[f].name.as_str()] = json!(v.as_ref());
                    }
                }
                for (&f, was_untold) in derived.iter().zip(untold) {
                    if let (true, Value::Str(t)) = (was_untold, &d[f]) {
                        ops[i]["doc"][schema.fields[f].name.as_str()] = json!(t);
                    }
                }
                for &f in &media {
                    if let Value::Str(t) = &d[f] {
                        ops[i]["doc"][schema.fields[f].name.as_str()] = json!(t);
                    }
                }
            }
        }
        if n > 0 {
            self.metrics.embedded("write", n, t0.elapsed().as_secs_f64());
        }
        Ok(())
    }

    /// Reads the synonym list at `path` if it exists; `PUT /v1/synonyms`
    /// writes it there.
    pub fn open_synonyms(&mut self, path: &Path) -> Result<(), ApiError> {
        if let Backend::Cluster(_) = self.backend {
            return Err(ApiError::bad("--synonyms goes to each shard server, which expands its own searches"));
        }
        self.synonyms_file = Some(path.to_path_buf());
        match std::fs::read_to_string(path) {
            Ok(text) => self.set_synonyms_text(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ApiError::bad(format!("read {}: {e}", path.display()))),
        }
    }

    /// The synonym list, if one is loaded.
    pub fn synonyms(&self) -> Option<Arc<Synonyms>> {
        self.synonyms.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_synonyms_text(&self, text: &str) -> Result<(), ApiError> {
        let s = Synonyms::parse(text)?;
        *self.synonyms.write().unwrap_or_else(|e| e.into_inner()) = (!s.is_empty() || !text.trim().is_empty()).then(|| Arc::new(s));
        Ok(())
    }

    /// Replaces the synonym list (and its file, if the server has one).
    pub fn put_synonyms(&self, text: &str) -> Result<usize, ApiError> {
        if let Backend::Cluster(_) = self.backend {
            return Err(ApiError::bad("a coordinator has no synonym list: each shard server takes --synonyms FILE"));
        }
        let parsed = Synonyms::parse(text)?;
        if let Some(path) = &self.synonyms_file {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| ApiError::internal(format!("write {}: {e}", path.display())))?;
        }
        let rules = parsed.rules();
        *self.synonyms.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(parsed));
        Ok(rules)
    }

    /// Where the synonym list is saved, if anywhere.
    pub fn synonyms_file(&self) -> Option<&Path> {
        self.synonyms_file.as_deref()
    }

    /// Sets where `POST /v1/admin/checkpoint` writes checkpoints, creating
    /// the directory if need be.
    pub fn set_backup_dir(&mut self, dir: &Path) -> Result<(), ApiError> {
        if let Backend::Cluster(_) = self.backend {
            return Err(ApiError::bad("--backup-dir goes to each shard server, which checkpoints its own database"));
        }
        std::fs::create_dir_all(dir).map_err(|e| ApiError::bad(format!("create {}: {e}", dir.display())))?;
        self.backup_dir = Some(dir.to_path_buf());
        Ok(())
    }

    /// Sets where `POST /v1/admin/slices` writes slices, creating the
    /// directory if need be.
    pub fn set_slices_dir(&mut self, dir: &Path) -> Result<(), ApiError> {
        if let Backend::Cluster(_) = self.backend {
            return Err(ApiError::bad("--slices goes to a server with a database of its own: a slice is built from one table's rows"));
        }
        self.slices = Some(Arc::new(crate::slices::Slices::new(dir)?));
        Ok(())
    }

    /// The slices this server builds and serves.
    pub fn slices(&self) -> Result<&Arc<crate::slices::Slices>, ApiError> {
        self.slices.as_ref().ok_or_else(|| ApiError::bad("slices need the server started with --slices DIR"))
    }

    /// The directory of a table's slice, if the server serves slices and
    /// the name is one a table may have.
    pub fn slice_dir(&self, table: &str) -> Result<PathBuf, ApiError> {
        self.slices()?.dir_of(table)
    }

    /// The options a table's slice was built with, from its manifest: what
    /// a rebuild (`POST /v1/admin/slices` with the table alone) takes.
    pub fn slice_options(&self, table: &str) -> Result<chloedb::slice::BuildOptions, ApiError> {
        let i = self.slices()?.info(table)?;
        Ok(chloedb::slice::BuildOptions { table: Some(i.table.clone()), key: i.key.clone() })
    }

    /// Builds the slice of a table ([`chloedb::slice::build`]) into its
    /// directory under `--slices`, replacing the one there only once the
    /// new one is whole, so that a device never reads half of one.
    pub async fn build_slice(&self, opts: chloedb::slice::BuildOptions) -> Result<chloedb::slice::Built, ApiError> {
        let db = self.admin_db()?;
        let slices = self.slices()?.clone();
        let table = match &opts.table {
            Some(t) => t.clone(),
            None => db.snapshot().schema().table.clone(),
        };
        let dest = slices.dir_of(&table)?;
        let parent = dest.parent().expect("under the slices directory").to_path_buf();
        tokio::task::spawn_blocking(move || {
            // The table's writes so far: a write after this one makes the
            // slice stale (one during the build too, which is safe).
            let ops = db.storage().activity(&table).map_or(0, |a| a.ops);
            let fresh = parent.join(format!(".building-{table}"));
            let old = parent.join(format!(".old-{table}"));
            let _ = std::fs::remove_dir_all(&fresh);
            let built = chloedb::slice::build(&db, &opts, &fresh)?;
            let _ = std::fs::remove_dir_all(&old);
            if dest.exists() {
                std::fs::rename(&dest, &old)?;
            }
            std::fs::rename(&fresh, &dest)?;
            let _ = std::fs::remove_dir_all(&old);
            slices.built(crate::slices::Info::of(&built.manifest, built.manifest_bytes), ops);
            Ok::<_, chloedb::Error>(built)
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(Into::into)
    }

    /// Rebuilds every slice whose table changed since it was built, with
    /// the options it was built with; the tables rebuilt.
    pub async fn rebuild_stale_slices(&self) -> Result<Vec<String>, ApiError> {
        let db = self.admin_db()?;
        let slices = self.slices()?.clone();
        let mut done = Vec::new();
        for table in slices.tables() {
            if !slices.is_fresh(&table, &db).unwrap_or(true) {
                self.build_slice(self.slice_options(&table)?).await?;
                done.push(table);
            }
        }
        Ok(done)
    }

    /// Writes a checkpoint of the database ([`chloedb::storage::Database::checkpoint`])
    /// named `name` in the backup directory, hard-linking the segments an
    /// earlier checkpoint there (`link_from`) already holds.
    pub async fn checkpoint(&self, name: &str, layout: Layout, link_from: Option<&str>) -> Result<CheckpointStats, ApiError> {
        let dir = self.backup_dir.as_ref().ok_or_else(|| ApiError::bad("checkpoints need the server started with --backup-dir"))?;
        let db = self.admin_db()?;
        let dest = dir.join(checkpoint_name(name)?);
        let link_from = link_from.map(|n| checkpoint_name(n).map(|n| dir.join(n))).transpose()?;
        let opts = CheckpointOptions { layout, link_from };
        tokio::task::spawn_blocking(move || db.storage().checkpoint(&dest, &opts)).await.map_err(ApiError::internal)?.map_err(Into::into)
    }

    /// Checks every byte of the database against its checksums
    /// ([`chloedb::storage::Database::check`]). Writes wait meanwhile.
    pub async fn check(&self) -> Result<(), ApiError> {
        let db = self.admin_db()?;
        // Its bytes may be whole, but it is not the database that was
        // written: say so until it is opened clean (or restored).
        if let Some(what) = db.storage().damaged() {
            return Err(ApiError::internal(format!("the database was opened past damage, and lacks what was dropped: {what}")));
        }
        let deadline = Instant::now() + self.max_timeout;
        tokio::task::spawn_blocking(move || wait_for_writer(deadline, || Ok(db.storage().check()?))).await.map_err(ApiError::internal)?
    }

    /// Returns free space to the file system ([`chloedb::storage::Database::vacuum`]);
    /// the bytes the file shrank by.
    pub async fn vacuum(&self) -> Result<u64, ApiError> {
        let db = self.admin_db()?;
        let deadline = Instant::now() + self.max_timeout;
        tokio::task::spawn_blocking(move || wait_for_writer(deadline, || Ok(db.storage().vacuum()?))).await.map_err(ApiError::internal)?
    }

    pub(crate) fn admin_db(&self) -> Result<Arc<Db>, ApiError> {
        self.local_db().cloned().ok_or_else(|| ApiError::bad("a coordinator has no database of its own: ask each shard server"))
    }

    /// A place in the write path, held until the request is answered;
    /// 429 when [`Self::max_queued_writes`] requests hold one.
    fn enqueue(&self) -> Result<InQueue, ApiError> {
        InQueue::start(&self.writer_state, self.max_queued_writes)
            .ok_or_else(|| ApiError { code: None, status: 429, message: format!("{} writes are queued, the most this server takes, so the write was not applied; send it again in a second", self.max_queued_writes) })
    }

    /// Hands a write to the writer thread and waits for its commit. The
    /// writer records `key`'s outcome (the key, and the hash of what it
    /// names), even if this future is dropped: in memory, and in the
    /// write's transaction ([`idem::record`]).
    /// Writes `moved` are rows of moving slots, copied by the coordinator
    /// moving them or deleted here ([`Self::check_keys`]); while slots move
    /// from this shard, a write to them, or an UPDATE or DELETE statement,
    /// holds the move's gate until it commits ([`Self::pass_gate`]).
    async fn write(&self, job: WriteJob, deadline: Instant, key: Option<(String, u64)>, queued: InQueue, moved: Moved) -> Result<Written, ApiError> {
        let tx = self.writes.as_ref().ok_or_else(|| ApiError::internal("no writer for a coordinator"))?;
        // Counted in before it looks at the map, which a move may change,
        // and before it looks for a hold of the shard's writes, which waits
        // for it: a client's write either commits before the hold or is
        // refused. A slot move's own writes go on, and the hold's.
        let entry = self.moving.enter();
        if matches!(moved, Moved::No) && !matches!(&job, WriteJob::Ops(ops) if ops.iter().any(feed::marks_hold)) {
            self.not_held()?;
        }
        let pass = if self.slots().is_none() {
            moving::Pass::Entry(entry)
        } else {
            let catalog = self.catalog();
            let places: Vec<(&str, Value)> = match &job {
                WriteJob::Ops(ops) => ops.iter().filter(|op| !is_mark(op)).map(|op| op_schema(&catalog, op).and_then(|t| Ok((place_name(t), op_place(t, op)?)))).collect::<Result<Vec<_>, _>>()?,
                WriteJob::Sql(p) => {
                    let what = place_name(p.table());
                    p.inserted_places().into_iter().map(|v| (what, v.clone())).collect()
                }
            };
            let statement = matches!(&job, WriteJob::Sql(_)) && places.is_empty();
            let pass = match moved {
                Moved::No => self.pass_gate(entry, &places, statement).await?,
                Moved::Copy | Moved::Cleanup => moving::Pass::Entry(entry),
            };
            self.check_keys(places.iter().map(|(w, v)| (*w, v)), moved)?;
            pass
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        let keys = self.idempotency.clone();
        let mark = match moved {
            Moved::No => None,
            Moved::Copy => Some((feed::MOVE_COPY, self.slot_epoch() as i64)),
            Moved::Cleanup => Some((feed::MOVE_CLEANUP, self.slot_epoch() as i64)),
        };
        // The term a primary takes it in; a server that stepped down since
        // it was admitted commits none of its clients' writes (term 0).
        let term = match (&self.replication(), moved) {
            (Replication::Primary(_), _) => Some(self.term()),
            (Replication::Replica(_), Moved::No) => Some(0),
            _ => None,
        };
        tx.send(Queued { job, deadline, reply: Some(reply), key, keys, queued: Some(queued), pass: Some(pass), mark, epoch: routed(), term }).map_err(|_| ApiError::internal("the writer thread stopped"))?;
        rx.await.map_err(|_| ApiError::internal("the writer thread stopped"))?
    }

    /// The database, unless this is a coordinator.
    pub fn local_db(&self) -> Option<&Arc<Db>> {
        self.local()
    }

    fn local(&self) -> Option<&Arc<Db>> {
        match &self.backend {
            Backend::Local(db) => Some(db),
            Backend::Cluster(_) => None,
        }
    }

    /// The database's tables, the first first: a local database's as they
    /// are now, a coordinator's as its shards last named them
    /// ([`Coordinator::catalog`]).
    pub fn catalog(&self) -> Vec<Arc<Schema>> {
        match &self.backend {
            Backend::Local(db) => db.catalog(),
            Backend::Cluster(c) => c.catalog(),
        }
    }

    /// The first table's schema as it is now: [`Self::schema`] is the one
    /// the server started with, before any ALTER TABLE.
    pub fn first_table(&self) -> Arc<Schema> {
        self.catalog().into_iter().next().unwrap_or_else(|| Arc::new(self.schema.clone()))
    }

    /// The table a request that names none reads or writes: the first
    /// table, or on a database made with no tables (`chloedb-server --db`
    /// on a new path, [`chloedb::db::Db::create_empty`]) the first one made
    /// after its placeholder. An error while there is none.
    pub fn default_table(&self) -> Result<Arc<Schema>, ApiError> {
        let first = self.first_table();
        if !first.is_placeholder() {
            return Ok(first);
        }
        chloedb::db::default_table(&self.catalog()).ok_or_else(|| ApiError::bad(NO_TABLES))
    }

    /// The tables people made, the first first: [`Self::catalog_now`]
    /// without the placeholder of a database made with no tables.
    /// The names of the tables, for a key limited to some
    /// ([`crate::scoped`]).
    pub fn table_names(&self) -> Vec<String> {
        self.catalog().iter().map(|t| t.table.clone()).collect()
    }

    pub async fn tables_listed(&self) -> Vec<Arc<Schema>> {
        self.catalog_now().await.into_iter().filter(|t| !t.is_placeholder()).collect()
    }

    /// The table named `name` (any case). A coordinator that does not know
    /// it asks its shards again first.
    pub async fn table(&self, name: &str) -> Result<Arc<Schema>, ApiError> {
        let find = |c: Vec<Arc<Schema>>| c.into_iter().find(|t| t.table.eq_ignore_ascii_case(name));
        if let Some(t) = find(self.catalog()) {
            return Ok(t);
        }
        if self.learn_tables(false).await {
            if let Some(t) = find(self.catalog()) {
                return Ok(t);
            }
        }
        Err(ApiError::bad(format!("query error: no such table: {name}")))
    }

    /// The tables as they are now: a coordinator asks its shards again
    /// first (at most once a second), so that a table another coordinator
    /// created is listed ([`Self::learn_tables`]).
    pub async fn catalog_now(&self) -> Vec<Arc<Schema>> {
        self.learn_tables(false).await;
        self.catalog()
    }

    /// A coordinator asks its shards for their tables and rank profiles
    /// again ([`Coordinator::refresh`]; at most once a second unless
    /// `now`): whether it learned of a change.
    pub(crate) async fn learn_tables(&self, now: bool) -> bool {
        let Backend::Cluster(c) = &self.backend else { return false };
        let changed = c.refresh(now).await;
        if changed {
            self.refresh_models();
        }
        changed
    }

    /// Every table's rank profiles, by table, then name: a local
    /// database's as they are now, a coordinator's as its shards last held
    /// them alike ([`Coordinator::rank_profiles`]).
    pub fn rank_profiles(&self) -> Vec<(String, RankProfile)> {
        match &self.backend {
            Backend::Local(db) => db.catalog().into_iter().flat_map(|t| db.storage().rank_profiles(&t.table).unwrap_or_default().into_iter().map(move |p| (t.table.clone(), p))).collect(),
            Backend::Cluster(c) => c.rank_profiles().to_vec(),
        }
    }

    /// Table `table`'s rank profile named `name`, both in any case.
    pub fn rank_profile(&self, table: &str, name: &str) -> Option<RankProfile> {
        let named = |p: &RankProfile| p.name.eq_ignore_ascii_case(name);
        match &self.backend {
            Backend::Local(db) => db.storage().rank_profiles(table).ok()?.into_iter().find(named),
            Backend::Cluster(c) => c.rank_profiles().iter().find(|(t, p)| t.eq_ignore_ascii_case(table) && named(p)).map(|(_, p)| p.clone()),
        }
    }

    /// The stored functions queries call ([`chloedb::query::function`]): a
    /// local database's as they are now, a coordinator's as its replicas
    /// held them alike when last asked ([`Coordinator::functions`]).
    pub fn functions(&self) -> Arc<FunctionSet> {
        match &self.backend {
            Backend::Local(db) => db.functions().unwrap_or_default(),
            Backend::Cluster(c) => c.functions(),
        }
    }

    /// A coordinator asks its shards for their functions again (at most
    /// once a second, as [`Self::catalog_now`] asks for tables).
    pub async fn learn_functions(&self) {
        self.learn_tables(false).await;
    }

    /// The tables a query may read: [`Self::catalog`], and a local
    /// database's system tables (`_functions`, `_sources`), which
    /// [`Db::query`] reads too.
    fn readable(&self) -> Vec<Arc<Schema>> {
        let mut c = self.catalog();
        if let Backend::Local(db) = &self.backend {
            let snap = db.snapshot();
            c.extend(snap.schemas().filter(|s| [FUNCTIONS, SOURCES].iter().any(|n| s.table.eq_ignore_ascii_case(n))).map(|s| Arc::new(s.clone())));
        }
        c
    }

    /// The table named `name` that a query reads ([`Self::table`]), a
    /// local database's system tables too ([`Self::readable`]).
    async fn readable_table(&self, name: &str) -> Result<Arc<Schema>, ApiError> {
        match self.readable().into_iter().find(|t| t.table.eq_ignore_ascii_case(name)) {
            Some(t) => Ok(t),
            None => self.table(name).await,
        }
    }

    /// The plan of a query `sql`, bound as [`Self::bound`] binds, over the
    /// tables a query may read ([`Self::readable`]), its rank profiles and
    /// its stored functions written out.
    async fn bound_read(self: &Arc<Self>, sql: &str, params: &Params, timeout: Duration) -> Result<Plan, ApiError> {
        // Texts where vectors go, embedded before the query is bound.
        let mut embedded = self.embed_texts(sql, params).await?;
        // A rank profile whose body takes text where a vector goes
        // (`similarity(emb, :q)`, `embed(:q)`): written out, its texts are
        // embedded as a query's are, and the plan names the profile as it
        // would have.
        let mut profiled = None;
        if embedded.is_none() && contains_word(sql, "rank_profile") {
            if let Some((written, r)) = self.written_profile(sql) {
                if let Some(e) = self.embed_texts(&written.1, params).await? {
                    embedded = Some(e);
                    profiled = Some((written.0, r));
                }
            }
        }
        let (sql, params) = match &embedded {
            Some((s, p)) => (s.as_str(), p),
            None => (sql, params),
        };
        let deadline = Instant::now() + timeout;
        let plan = match self.plan_read(sql, params, deadline).await {
            Err(e) if (["no such table", "has no rank profile named", "unknown function"].iter().any(|m| e.message.contains(m))) && self.learn_tables(false).await => self.plan_read(sql, params, deadline).await?,
            r => r?,
        };
        Ok(match profiled {
            Some((table, r)) => profile::mark(plan, &table, &r),
            None => plan,
        })
    }

    /// The plan of the read `sql` over the tables a query may read
    /// ([`compound::plan_read_calling`]), stopped at `deadline` or once
    /// the caller stops waiting for it. A short statement plans here; a
    /// longer one, whose planning may take long (thousands of `WITH`
    /// results, a long chain of `UNION`s), on a blocking thread, so that
    /// it holds none of the threads that serve the other requests, and a
    /// deep one with the stack for it.
    async fn plan_read(self: &Arc<Self>, sql: &str, params: &Params, deadline: Instant) -> Result<Plan, ApiError> {
        let cancel = Cancel::default();
        let opts = ExecOptions { deadline: Some(deadline), cancel: Some(cancel.0.clone()), ..ExecOptions::default() };
        let deep = chloedb::query::parse::nests_deep(sql);
        let plan = move |me: &Service, sql: &str, params: &Params| {
            chloedb::query::parse::deep_stack(deep, || {
                let functions = me.functions();
                let c = me.readable();
                let refs: Vec<&Schema> = c.iter().map(|t| &**t).collect();
                compound::planning_under(&opts, || compound::plan_read_calling(sql, &refs, params, &|t, n| me.rank_profile(t, n), &functions))
            })
        };
        if sql.len() <= PLAN_INLINE_BYTES {
            return Ok(plan(self, sql, params)?);
        }
        let (me, sql, params) = (self.clone(), sql.to_string(), params.clone());
        // Dropped with this future (the client went away), `cancel` stops
        // the planning at its next step.
        let r = tokio::task::spawn_blocking(move || plan(&me, &sql, &params)).await.map_err(ApiError::internal)?;
        drop(cancel);
        Ok(r?)
    }

    /// Query `sql` written out with the rank profile it ranks by
    /// ([`profile::expand`]): its table and text, and the profile; `None`
    /// when it ranks by none (or does not parse: the plan says why).
    fn written_profile(&self, sql: &str) -> Option<((String, String), chloedb::query::ir::ProfileRef)> {
        let statements = chloedb::query::parse::parse(sql).ok()?;
        let [sqlparser::ast::Statement::Query(q)] = statements.as_slice() else { return None };
        let (out, r) = profile::expand(q, &|t, n| self.rank_profile(t, n)).ok()??;
        let table = crate::scoped::named_tables(sql).into_iter().next()?;
        Some(((table, out.to_string()), r))
    }

    /// Query `sql` with the texts it gives where vectors go embedded
    /// ([`chloedb::query::embed_sql`]: `similarity(emb, 'some words')` by
    /// the field's model, `embed('model', text)` by a model named), on the
    /// embedding cores as a parameter's `{"embed": ..}` is, and its
    /// parameters with their vectors; `None` when it has none.
    async fn embed_texts(&self, sql: &str, params: &Params) -> Result<Option<(String, Params)>, ApiError> {
        use chloedb::query::embed_sql::{self, Request};
        if !embed_sql::may_embed_with(sql, params) {
            return Ok(None);
        }
        let catalog = self.readable();
        let refs: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
        let requests = embed_sql::requests(sql, &refs, params)?;
        if requests.is_empty() {
            return Ok(None);
        }
        let mut given: HashMap<Request, Value> = HashMap::new();
        for r in requests {
            if given.contains_key(&r) {
                continue;
            }
            let (e, tokens, text) = match &r {
                Request::Field { table, field, text } => {
                    let (_, _, e) = self.model(Some(table), Some(field))?;
                    let tokens = refs.iter().find(|s| s.table.eq_ignore_ascii_case(table)).and_then(|s| s.field(field)).is_some_and(|f| f.multi);
                    (e, tokens, text.clone())
                }
                Request::Model { model, text } => (self.named_model(model)?, false, text.clone()),
            };
            let t0 = Instant::now();
            let core = match tokio::time::timeout(self.timeout, self.embedding.clone().acquire_owned()).await {
                Ok(p) => p.map_err(ApiError::internal)?,
                Err(_) => return Err(ApiError { code: None, status: 429, message: "every core is embedding; try again".into() }),
            };
            let v = tokio::task::spawn_blocking(move || {
                let _core = core;
                match tokens {
                    true => e.embed_query_tokens(&text).map(|vs| Value::List(vs.into_iter().map(|v| Value::Vector(v.into())).collect())),
                    false => e.embed_query(&text).map(|v| Value::Vector(v.into())),
                }
            })
            .await
            .map_err(ApiError::internal)??;
            self.metrics.embedded("query", 1, t0.elapsed().as_secs_f64());
            given.insert(r, v);
        }
        Ok(embed_sql::written(sql, &refs, params, &given)?)
    }

    /// The model a query names in `embed('model', ..)`: one an embedded
    /// field has open, `hash:N`, or a model directory directly under the
    /// models directory, opened once and kept. A path, or a model behind
    /// an API that no field uses, is refused: a query may not make the
    /// server read other files, or spend its keys.
    fn named_model(&self, name: &str) -> Result<Arc<dyn Embedder>, ApiError> {
        if let Some(e) = self.models().iter().filter(|m| m.spec.model.eq_ignore_ascii_case(name)).find_map(|m| m.model.as_ref().ok()) {
            return Ok(e.clone());
        }
        if let Some(e) = self.named_models.lock().unwrap_or_else(|e| e.into_inner()).get(name) {
            return Ok(e.clone());
        }
        let refuse = |why: &str| ApiError::bad(format!("query error: embed('{name}', ..): {why}"));
        let Some(dir) = self.models_dir.clone() else { return Err(refuse("this server has no models directory (--models)")) };
        if let Some(why) = chloedb::query::embed_sql::refuse_model(name, &dir) {
            return Err(refuse(why));
        }
        let e = chloedb::embed::open(name, &dir).map_err(|e| match chloedb::embed::KNOWN_MODELS.iter().any(|k| k.0.eq_ignore_ascii_case(name)) {
            true => refuse(&format!("{} (chloedb-server fetch-model {name} downloads it)", e.to_string().trim_start_matches("schema error: "))),
            false => refuse(e.to_string().trim_start_matches("schema error: ")),
        })?;
        if !e.single() {
            return Err(refuse("it embeds a text into a vector a token: embed() needs a model of one vector a text"));
        }
        let mut kept = self.named_models.lock().unwrap_or_else(|e| e.into_inner());
        // A few: each is a model's tokenizer and mapped table.
        if kept.len() >= 16 {
            kept.clear();
        }
        kept.insert(name.to_string(), e.clone());
        Ok(e)
    }

    /// Runs `bind` over the database's tables; a coordinator whose tables
    /// lack one the statement names, or a rank profile it ranks by, asks
    /// its shards again, then binds once more.
    async fn bound<T>(&self, bind: impl Fn(&[&Schema]) -> chloedb::Result<T>) -> Result<T, ApiError> {
        let run = || {
            let c = self.catalog();
            let refs: Vec<&Schema> = c.iter().map(|t| &**t).collect();
            bind(&refs)
        };
        match run() {
            Err(e) if (e.to_string().contains("no such table") || e.to_string().contains("has no rank profile named")) && self.learn_tables(false).await => Ok(run()?),
            r => Ok(r?),
        }
    }

    /// Pins the current version ([`Self::latest`]) for `lease_ttl`;
    /// returns the lease and the version's txn.
    pub fn lease(&self) -> Result<(u64, u64), ApiError> {
        let db = self.local().ok_or_else(|| ApiError::bad("a coordinator holds no snapshots"))?;
        let snap = self.latest(db)?;
        let txn = snap.txn();
        Ok((self.lease_of(snap, self.lease_ttl)?, txn))
    }

    /// Keeps `snap` for `ttl`; returns the lease. At most [`MAX_LEASES`]
    /// are held at once.
    fn lease_of(&self, snap: Snapshot, ttl: Duration) -> Result<u64, ApiError> {
        let mut l = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        l.retain(|_, x| x.until > now);
        if l.len() >= MAX_LEASES {
            return Err(ApiError { code: None, status: 429, message: format!("{MAX_LEASES} snapshot leases are held; try again when some expire") });
        }
        let id = self.next_lease.fetch_add(1, Ordering::Relaxed);
        l.insert(id, Lease { snap, until: now + ttl, expansions: Arc::default() });
        Ok(id)
    }

    /// The version of the BM25 statistics of the table `snap` looks at: of
    /// the table's text statistics generation ([`Snapshot::stats_gen`]),
    /// which a write to it that adds a row, a flush or a merge moves on but
    /// values set in place, deletes and other tables' writes do not, and of
    /// this run of the server. Never 0, which a query sends for any version.
    pub fn stats_version(&self, snap: &Snapshot) -> u64 {
        match self.stats_nonce ^ snap.stats_gen() {
            0 => 1,
            v => v,
        }
    }

    /// The typo expansions found on lease `lease`'s snapshot so far, which
    /// the queries reading it share (a coordinator's statistics round,
    /// then its query round): `None` for no lease, or one unknown.
    fn lease_expansions(&self, lease: u64) -> Option<Arc<Expansions>> {
        if lease == 0 {
            return None;
        }
        self.leases.lock().unwrap_or_else(|e| e.into_inner()).get(&lease).map(|x| x.expansions.clone())
    }

    /// Keeps lease `id` for `ttl` from now, if it is held.
    fn renew_lease(&self, id: u64, ttl: Duration) {
        if let Some(x) = self.leases.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&id) {
            x.until = x.until.max(Instant::now() + ttl);
        }
    }

    /// Ends `leases` before they expire, and releases the versions only
    /// they held. Leases unknown or expired are passed over.
    pub fn release(&self, leases: &[u64]) {
        let mut l = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        for id in leases {
            l.remove(id);
        }
    }

    /// The lease a query reads with `hold` ([`QueryReq::hold`]): the one
    /// it names, or a new one on `snap`, the version it reads, lasting as
    /// a [`Service::term_stats`] lease.
    fn held(&self, req: &QueryReq, snap: &Snapshot, timeout: Duration) -> Result<u64, ApiError> {
        match (req.hold, req.snapshot) {
            (true, 0) => self.lease_of(snap.clone(), self.lease_ttl.min(timeout + Duration::from_secs(1))),
            (true, l) => Ok(l),
            (false, _) => Ok(0),
        }
    }

    /// Releases expired leases, and the versions only they held; returns
    /// how many are held still. The maintenance loop calls it, so that a
    /// version is not kept until the next lease is made.
    pub fn sweep_leases(&self) -> usize {
        let mut l = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        l.retain(|_, x| x.until > now);
        l.len()
    }

    /// A request's deadline: its `timeout_ms` up to [`Self::max_timeout`],
    /// or the default.
    pub fn timeout_of(&self, timeout_ms: u64) -> Duration {
        if timeout_ms > 0 {
            let t = Duration::from_millis(timeout_ms).min(self.max_timeout);
            if t < self.timeout {
                let _ = DEADLINE.try_with(|d| d.asked_ms.set(Some(t.as_millis() as u64)));
            }
            t
        } else {
            self.timeout
        }
    }

    /// The version lease `lease` keeps.
    fn leased(&self, lease: u64) -> Result<Snapshot, ApiError> {
        let l = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        match l.get(&lease) {
            Some(x) if x.until > Instant::now() => Ok(x.snap.clone()),
            _ => Err(ApiError::bad(format!("snapshot lease {lease} is unknown or expired"))),
        }
    }

    /// The latest version, for a read that does not name a lease: refused
    /// on a replica its primary has not found caught up (503,
    /// [`Follow::readable`]).
    ///
    /// The version is taken under the cluster map it is read by: rows of
    /// moving slots this shard holds without serving them are hidden
    /// ([`Self::hide_residue`]), and a read routed by a map this shard has
    /// left since its epoch was checked is refused (409), as the rows it
    /// would read are another map's.
    fn latest(&self, db: &Db) -> Result<Snapshot, ApiError> {
        match &self.replication() {
            Replication::Replica(f) => f.readable()?,
            Replication::Primary(p) => p.readable()?,
            Replication::None => {}
        }
        let held = self.slots.read().unwrap_or_else(|e| e.into_inner());
        self.epoch_ok(held.as_ref().map_or(0, |(m, _)| m.epoch), routed())?;
        let snap = db.snapshot();
        let mask = self.moving.mask();
        drop(held);
        match mask {
            Some(m) => Ok(snap.masked(&m)?),
            None => Ok(snap),
        }
    }

    /// The version a read reads: lease `lease`, or the latest
    /// ([`Self::latest`]), holding the write `after` names. A replica waits
    /// until it has applied that write, until `deadline` and
    /// [`READ_WAIT`] at most, and refuses the read then (503: read from
    /// the primary). A primary, or a server without replicas, holds every
    /// write it answered, so it refuses a number past its last (409), and
    /// a lease keeps its version, so one from before the write is refused
    /// too (409).
    async fn read_snapshot(&self, db: &Db, lease: u64, after: &After, deadline: Instant) -> Result<Snapshot, ApiError> {
        let min = after.seq()?;
        if min > 0 && db.storage().numbering().is_none() {
            return Err(ApiError::bad("min_seq_no needs a database that numbers its writes: start the server with --seq-no"));
        }
        let snap = match lease {
            0 => self.latest(db)?,
            l => self.leased(l)?,
        };
        let at = snap.seq_no().unwrap_or(0);
        if at >= min {
            return Ok(snap);
        }
        match (&self.replication(), lease) {
            (Replication::Replica(f), 0) => f.holding(min, deadline.min(Instant::now() + READ_WAIT)).await,
            (_, 0) => Err(conflict(format!("this server's last transaction is {at}: transaction {min} was not committed here"))),
            (_, l) => Err(conflict(format!("snapshot lease {l} keeps a version holding transactions up to {at}, before {min}"))),
        }
    }


    /// Parameters from JSON. Besides plain values (see [`js::param`]), a
    /// parameter may be `{"vector_of": key, "field": "emb"}`, the vector of
    /// a stored document (field: the first VECTOR field if left out), or
    /// `{"embed": "text", "field": "emb"}`, the vector of the text by the
    /// model of that embedded field (the first one with a model if left
    /// out), so that the text and the rows are embedded alike, or
    /// `{"embed": {"image": "<base64>"}, "field": "emb"}` (or `"audio"`),
    /// the vector of an image or sound file by a model of that and text.
    /// The text of a multi-vector field (`VECTOR(d)[]`) embedded by a
    /// model of its tokens, such as ColBERT, is the list of its tokens'
    /// vectors, as `maxsim` takes them; that of a SPARSE field embedded by
    /// a learned sparse model, its term weights, as `similarity` takes them.
    ///
    /// `who` is the request's credential (`None`: the server's own caller,
    /// an admin's route, whose scope was checked already).
    pub async fn params(self: &Arc<Self>, who: Option<&Principal>, j: Option<&Json>) -> Result<Params, ApiError> {
        let open = Principal::open();
        self.params_for(j, None, who.unwrap_or(&open)).await
    }

    /// [`Self::params`] of statement `sql` (when there is one), for `who`.
    /// A `{"vector_of"}` or `{"embed"}` parameter that names no `table`
    /// resolves against the tables the statement names: the one of them
    /// with such a field, refused when several have one; and only when
    /// none has, the first table of the database with one (of those `who`
    /// may read). Whatever table it reads (a row's vector, the model of a
    /// field, whose provider's key it may spend), `who` reads it: a key
    /// limited to tables reads no other through a parameter.
    pub async fn params_for(self: &Arc<Self>, j: Option<&Json>, sql: Option<&str>, who: &Principal) -> Result<Params, ApiError> {
        let mut p = Params::new();
        let Some(j) = j.filter(|j| !j.is_null()) else { return Ok(p) };
        let m = j.as_object().ok_or_else(|| ApiError::bad("params must be an object"))?;
        let named: Vec<String> = sql.map(crate::scoped::named_tables).unwrap_or_default();
        for (k, v) in m {
            let value = match v {
                Json::Object(o) if o.contains_key("vector_of") && o.keys().all(|k| ["vector_of", "field", "table"].contains(&k.as_str())) => {
                    let field = o.get("field").and_then(|f| f.as_str());
                    let catalog = self.catalog();
                    let has = |t: &str| {
                        catalog.iter().find(|s| s.table.eq_ignore_ascii_case(t)).is_some_and(|s| match field {
                            Some(f) => s.field(f).is_some(),
                            None => s.fields.iter().any(|f| matches!(f.ty, chloedb::FieldType::Vector { .. })),
                        })
                    };
                    let order: Vec<String> = catalog.iter().map(|s| s.table.clone()).collect();
                    if let Some(t) = o.get("table").and_then(|f| f.as_str()) {
                        who.may_table(Scope::Read, t, &format!("parameter :{k} ({{\"vector_of\"}} reads a row)"))?;
                    }
                    let table = param_table(o.get("table").and_then(|f| f.as_str()), &named, &order, who, &has, "vector_of").map_err(|e| ApiError { message: format!("parameter :{k}: {}", e.message), ..e })?;
                    if table.is_none() && who.limited() {
                        return Err(ApiError::bad(format!("parameter :{k}: no table the key {:?} may read has {}", who.name, field.map_or("a VECTOR field".to_string(), |f| format!("a field {f}")))));
                    }
                    let (table, field) = self.vector_field(table.as_deref(), field)?;
                    who.may_table(Scope::Read, &table, &format!("parameter :{k} ({{\"vector_of\"}} reads a row)"))?;
                    self.vector_of(&table, &o["vector_of"], &field).await?
                }
                Json::Object(o) if o.get("embed").is_some_and(|e| e.is_string() || e.get("image").is_some() || e.get("audio").is_some()) && o.keys().all(|k| ["embed", "field", "table"].contains(&k.as_str())) => {
                    let (table, _) = self.embed_target(o.get("table").and_then(|t| t.as_str()), o.get("field").and_then(|f| f.as_str()), &named, who).map_err(|e| ApiError { message: format!("parameter :{k}: {}", e.message), ..e })?;
                    let (table, field, e) = self.model(Some(&table), o.get("field").and_then(|f| f.as_str()))?;
                    // A multi-vector field's model embeds the query's tokens.
                    let tokens = self.catalog().iter().find(|s| s.table.eq_ignore_ascii_case(&table)).and_then(|s| s.field(&field)).is_some_and(|f| f.multi);
                    let file = match &o["embed"] {
                        Json::String(_) => None,
                        Json::Object(i) if i.len() == 1 => {
                            let (kind, b64) = i.iter().next().expect("one key");
                            let b64 = b64.as_str().ok_or_else(|| ApiError::bad(format!("{{\"embed\": {{\"{kind}\": ...}}}} is a file in base64")))?;
                            Some(chloedb::embed::query_file(kind, b64)?)
                        }
                        _ => return Err(ApiError::bad("\"embed\" is a text, {\"image\": \"<base64>\"} or {\"audio\": \"<base64>\"}")),
                    };
                    if let Some((m, _)) = file.as_ref().filter(|(m, _)| !e.modalities().contains(m)) {
                        return Err(ApiError::bad(format!("the model of {field} ({}) does not embed {}", e.name(), chloedb::embed::modality_name(*m))));
                    }
                    if tokens && file.is_some() {
                        return Err(ApiError::bad(format!("{field} is a multi-vector field: its model embeds a text's tokens, not a file")));
                    }
                    let text = o["embed"].as_str().unwrap_or_default().to_string();
                    let t0 = Instant::now();
                    let core = match tokio::time::timeout(self.timeout, self.embedding.clone().acquire_owned()).await {
                        Ok(p) => p.map_err(ApiError::internal)?,
                        Err(_) => return Err(ApiError { code: None, status: 429, message: "every core is embedding; try again".into() }),
                    };
                    let v = tokio::task::spawn_blocking(move || {
                        let _core = core;
                        match file {
                            Some((_, data)) => chloedb::embed::embed_file(e.as_ref(), &data).map(|v| Value::Vector(v.into())),
                            None if e.sparse() => e.embed_query_sparse(&text).map(|t| Value::Str(chloedb::embed::terms_json(&t))),
                            None if tokens => e.embed_query_tokens(&text).map(|vs| Value::List(vs.into_iter().map(|v| Value::Vector(v.into())).collect())),
                            None => e.embed_query(&text).map(|v| Value::Vector(v.into())),
                        }
                    })
                    .await
                    .map_err(ApiError::internal)??;
                    self.metrics.embedded("query", 1, t0.elapsed().as_secs_f64());
                    v
                }
                v => js::param(v)?,
            };
            p = p.set(k, value);
        }
        Ok(p)
    }

    /// The table and embedded field whose model embeds a text for `who`
    /// (`{"embed"}`, `/v1/embed`): `field` of `table`; with no `table`, of
    /// the one of the tables a statement names (`named`, lowercase) with
    /// such a field, refused when several have one, else of the first
    /// table that has one and `who` may read. `who` reads the table: its
    /// model is the table's, and may spend its provider's key.
    pub fn embed_target(&self, given: Option<&str>, field: Option<&str>, named: &[String], who: &Principal) -> Result<(String, String), ApiError> {
        // A table created since the models were listed.
        let listed = self.models();
        let unlisted = |t: &str| !listed.iter().any(|m| m.table.eq_ignore_ascii_case(t));
        let embeds = |t: &str| self.catalog().iter().any(|s| s.table.eq_ignore_ascii_case(t) && s.fields.iter().any(|f| f.embed.is_some()));
        if given.into_iter().chain(named.iter().map(String::as_str)).any(|t| unlisted(t) && embeds(t)) {
            self.refresh_models();
        }
        let models = self.models();
        let has = |t: &str| {
            models.iter().any(|m| {
                m.table.eq_ignore_ascii_case(t)
                    && match field {
                        Some(f) => m.field.eq_ignore_ascii_case(f),
                        None => m.model.is_ok(),
                    }
            })
        };
        let mut order: Vec<String> = Vec::new();
        for m in models.iter() {
            if !order.iter().any(|t| t.eq_ignore_ascii_case(&m.table)) {
                order.push(m.table.clone());
            }
        }
        if let Some(t) = given {
            who.may_table(Scope::Read, t, "a text embedded by a table's model")?;
        }
        let table = param_table(given, named, &order, who, &has, "embed")?;
        let none = || match (who.limited(), field) {
            (true, Some(f)) => ApiError::bad(format!("no table the key {:?} may read has an embedded field {f}; pass a vector", who.name)),
            (true, None) => ApiError::bad(format!("no table the key {:?} may read has a field this server can embed text for; pass a vector", who.name)),
            (false, _) => ApiError::bad("no table has a field this server can embed text for; pass a vector"),
        };
        let table = match table {
            Some(t) => t,
            None if who.limited() => return Err(none()),
            None => return self.model(None, field).map(|(t, f, _)| (t, f)),
        };
        let (table, field, _) = self.model(Some(&table), field)?;
        who.may_table(Scope::Read, &table, &format!("a text embedded by the model of {table}.{field}"))?;
        Ok((table, field))
    }

    /// The table and VECTOR field `{"vector_of": key}` reads: `field` of
    /// `table`, or of the first table that has a field so named; with no
    /// `field`, the first VECTOR field of `table` (or of the first table).
    pub fn vector_field(&self, table: Option<&str>, field: Option<&str>) -> Result<(String, String), ApiError> {
        let catalog = self.catalog();
        let tables: Vec<&Arc<Schema>> = match table {
            Some(t) => vec![catalog.iter().find(|s| s.table.eq_ignore_ascii_case(t)).ok_or_else(|| ApiError::bad(format!("query error: no such table: {t}")))?],
            None => match field {
                Some(_) => catalog.iter().collect(),
                None => catalog.iter().take(1).collect(),
            },
        };
        for t in tables {
            let found = match field {
                Some(f) => t.field(f),
                None => t.fields.iter().find(|f| matches!(f.ty, chloedb::FieldType::Vector { .. })),
            };
            if let Some(f) = found {
                return Ok((t.table.clone(), f.name.clone()));
            }
        }
        Err(match field {
            Some(f) => ApiError::bad(format!("no field {f}")),
            None => ApiError::bad("the table has no VECTOR field"),
        })
    }

    /// The vector `field` of the document `key` of `table`.
    pub async fn vector_of(self: &Arc<Self>, table: &str, key: &Json, field: &str) -> Result<Value, ApiError> {
        let schema = self.table(table).await?;
        let pk = schema.primary_key.ok_or_else(|| ApiError::bad("vector_of needs a primary key"))?;
        let f = schema.field_index(field).ok_or_else(|| ApiError::bad(format!("no field {field}")))?;
        if !matches!(schema.fields[f].ty, chloedb::FieldType::Vector { .. }) {
            return Err(ApiError::bad(format!("{field} is not a VECTOR field")));
        }
        // Quoted: a field may be named like a keyword (`order`).
        let sql = format!("SELECT {} FROM {} WHERE {} = :k LIMIT 1", quoted(field), ident(&schema.table), quoted(&schema.fields[pk].name));
        let params = Params::new().set("k", js::field_value(&schema, pk, key)?);
        let a = self.query(QueryReq { sql, params, ..Default::default() }).await?;
        match a.rs.rows.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(v @ Value::Vector(_)) => Ok(v),
            Some(_) => Err(ApiError::bad(format!("document {key} has no {field}"))),
            None => Err(ApiError::bad(format!("no document {key}"))),
        }
    }

    /// The type and bytes of the file whose SHA-256 is `sha256`, found
    /// through the first row of a MEDIA field that names it and read as
    /// writes read files (the media store, or a place `--media-fetch`
    /// lists), checked against the hash. Only files rows name can be
    /// read. A coordinator refuses: its shards hold the files.
    pub async fn media_file(self: &Arc<Self>, sha256: &str) -> Result<(String, Vec<u8>), ApiError> {
        if !chloedb::media::is_sha256(sha256) {
            return Err(ApiError::bad("a file is named by the SHA-256 of its bytes: 64 lowercase hex digits"));
        }
        let Backend::Local(db) = &self.backend else {
            return Err(ApiError::bad("a coordinator holds no files: ask the shard server whose rows name it"));
        };
        let catalog = self.catalog();
        for schema in catalog.iter() {
            for f in schema.fields.iter().filter(|f| f.media.is_some()) {
                let sql = format!("SELECT {} FROM {} WHERE {} = :h LIMIT 1", quoted(&f.name), ident(&schema.table), quoted(&f.name));
                let params = Params::new().set("h", Value::Str(sha256.into()));
                let a = self.query(QueryReq { sql, params, ..Default::default() }).await?;
                let Some(Value::Str(text)) = a.rs.rows.into_iter().next().and_then(|r| r.into_iter().next()) else { continue };
                let m = chloedb::media::Media::parse(&text).map_err(ApiError::internal)?;
                let resolver = db.media_resolver();
                let mime = m.mime.clone();
                let bytes = tokio::task::spawn_blocking(move || resolver.bytes_of(&m)).await.map_err(ApiError::internal)??;
                return Ok((mime, bytes));
            }
        }
        Err(ApiError { code: None, status: 404, message: format!("no row names a file of sha256 {sha256}") })
    }

    /// The gazetteers of keyword fields that query understanding reads: a
    /// server's from its database, read once per version of the table; a
    /// coordinator's by a query of its shards, kept for
    /// [`GAZETTEER_TTL`].
    pub async fn gazetteers(self: &Arc<Self>, needs: &[(String, String, chloedb::text::Analyzer)], like: Option<&QueryReq>) -> Result<chloedb::query::understand::Gazetteers, ApiError> {
        if let Backend::Local(db) = &self.backend {
            let (db, needs) = (db.clone(), needs.to_vec());
            return Ok(tokio::task::spawn_blocking(move || db.gazetteers(&needs)).await.map_err(ApiError::internal)??);
        }
        let mut out = chloedb::query::understand::Gazetteers::new();
        for key in needs {
            if let Some((at, g)) = self.gazetteers.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
                if at.elapsed() < GAZETTEER_TTL {
                    out.insert(key.clone(), g.clone());
                    continue;
                }
            }
            let mut r = like.cloned().unwrap_or_default();
            // The server's own read: a dictionary of up to
            // MAX_LISTED_VALUES values, past --max-rows.
            (r.sql, r.params, r.scroll, r.hold, r.own_read) = (chloedb::query::understand::dictionary_sql(&key.0, &key.1), Params::new(), None, false, true);
            let rs = Box::pin(self.query(r)).await?.rs;
            let g = Arc::new(chloedb::query::understand::Gazetteer::build(&key.1, &rs, key.2));
            self.gazetteers.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), (Instant::now(), g.clone()));
            out.insert(key.clone(), g);
        }
        Ok(out)
    }

    /// What the words of `text` say for `field` of `table`
    /// ([`chloedb::db::Db::understand`]), on a server or a coordinator.
    pub async fn understand(self: &Arc<Self>, table: &str, field: &str, text: &str, opts: &chloedb::query::understand::Options) -> Result<chloedb::query::understand::Understood, ApiError> {
        let catalog = self.catalog();
        let schema = catalog.iter().find(|s| s.table.eq_ignore_ascii_case(table)).ok_or_else(|| ApiError::bad(format!("no table {table}")))?;
        let gaz = self.gazetteers(&chloedb::query::understand::needs_of(schema, field, opts)?, None).await?;
        let mut u = chloedb::query::understand::understand_in(schema, field, text, opts, &gaz)?;
        let counts = self.understanding_counts(&u, None).await?;
        u.confirm(&counts);
        Ok(u)
    }

    /// "Did you mean" ([`chloedb::db::Db::correct`]): the words of `text`
    /// in no document of TEXT field `field` of `table`, each replaced by
    /// the indexed term it most likely meant. A coordinator counts the
    /// documents by its shards' term statistics, summed: the words' own,
    /// then, for the words in none, their neighbours' (`fuzzy => n`, the
    /// nearest of each shard), and picks as one server does.
    pub async fn correct(self: &Arc<Self>, table: &str, field: &str, text: &str, timeout_ms: u64) -> Result<chloedb::db::Corrected, ApiError> {
        use chloedb::query::exec::{correction_words, corrected_text};
        if let Backend::Local(db) = &self.backend {
            let (db, table, field, text) = (db.clone(), table.to_string(), field.to_string(), text.to_string());
            return Ok(tokio::task::spawn_blocking(move || db.correct(&table, &field, &text)).await.map_err(ApiError::internal)??);
        }
        let schema = self.table(table).await?;
        let f = schema.field(field).ok_or_else(|| ApiError::bad(format!("{table} has no field {field}")))?;
        if !matches!(f.ty, chloedb::schema::FieldType::Text { .. }) {
            return Err(ApiError::bad(format!("{table}.{field} is not a TEXT field: only text is corrected")));
        }
        let analyzer = f.text_analyzer();
        let words = correction_words(text, analyzer);
        let unchanged = || chloedb::db::Corrected { text: text.to_string(), changed: false, words: Vec::new() };
        if words.is_empty() {
            return Ok(unchanged());
        }
        // A plain query of the table: a column, not an expression, which
        // would make it a query of operators.
        let col = quoted(&schema.primary_key.map_or(&f.name, |k| &schema.fields[k].name));
        let sql = |opts: &str| format!("SELECT {col} FROM {} ORDER BY bm25({}, :q{opts}) DESC LIMIT 1", ident(&schema.table), quoted(&f.name));
        let stats_of = |sql: String, q: String| {
            let this = self.clone();
            async move {
                let params = Params::new().set("q", Value::Str(q));
                let (st, _, _) = this.term_stats(&sql, None, &params, &After::default(), timeout_ms).await?;
                Ok::<_, ApiError>(st.fields.into_values().next().unwrap_or_default().df)
            }
        };
        // The words' own counts, in one round.
        let own = stats_of(sql(""), words.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(" ")).await?;
        let mut out = Vec::new();
        for w in &words {
            // A term the round did not count (a stopword of the ranking)
            // is left as written.
            let Some(&df) = own.get(&w.term) else { continue };
            if df > 0 {
                continue;
            }
            let near = stats_of(sql(&format!(", fuzzy => {}", w.edits)), w.word.clone()).await?;
            let typed: Vec<char> = w.term.chars().collect();
            let near: Vec<(String, u64, u8)> = near.into_iter().filter(|(n, _)| *n != w.term).filter_map(|(n, ndf)| chloedb::text::fuzzy::within(&typed, &n, w.edits).map(|d| (n, ndf, d))).collect();
            out.extend(w.correct(df, &near, analyzer));
        }
        if out.is_empty() {
            return Ok(unchanged());
        }
        Ok(chloedb::db::Corrected { text: corrected_text(text, &out), changed: true, words: out })
    }

    /// The answers of [`chloedb::query::understand::Understood::checks`]'s queries.
    async fn understanding_counts(self: &Arc<Self>, u: &chloedb::query::understand::Understood, like: Option<&QueryReq>) -> Result<Vec<u64>, ApiError> {
        let mut out = Vec::new();
        for c in u.checks() {
            let mut r = like.cloned().unwrap_or_default();
            (r.sql, r.params, r.scroll, r.hold, r.own_read) = (c, Params::new(), None, false, true);
            out.push(chloedb::query::understand::count_of(&Box::pin(self.query(r)).await?.rs));
        }
        Ok(out)
    }

    /// A query error (400, `query_invalid`) when the statement `sql` a
    /// request sent is longer than [`Self::max_statement_bytes`] or holds
    /// more than [`Self::max_statement_elements`] elements, before any of
    /// it is parsed.
    pub fn statement_fits(&self, sql: &str) -> Result<(), ApiError> {
        Ok(chloedb::query::parse::check_size(sql, self.max_statement_bytes, self.max_statement_elements, "the server's --max-statement-bytes and --max-statement-elements")?)
    }

    /// Runs a query, or its plan when it has one. A coordinator plans a
    /// query by a rank profile with the profile its shards held when it
    /// last asked; when a shard refuses the plan because its profile has
    /// changed since (through another coordinator, or on that shard), the
    /// coordinator asks again and plans once more.
    pub async fn query(self: &Arc<Self>, req: QueryReq) -> Result<Answer, ApiError> {
        if req.plan.is_none() {
            self.statement_fits(&req.sql)?;
        }
        let skip = req.skip_steps;
        let mut a = self.query_steps(req).await?;
        if skip {
            a.rs.stats.steps = Vec::new();
            a.rs.stats.steps_dropped = 0;
        }
        Ok(a)
    }

    /// The key of `req`'s answer in the answer cache ([`crate::answers`]),
    /// or `None` when it is not cached: no cache, a request that named no
    /// query ([`QueryReq::cache_key`]), or one whose answer holds more
    /// than its rows (a lease, a scroll, Arrow columns, measured steps).
    fn answer_key(&self, req: &QueryReq, table: &str) -> Option<crate::answers::Key> {
        let base = req.cache_key?;
        self.answers.as_ref()?;
        if req.hold || req.scroll.is_some() || req.profile || req.quiet || req.arrow.is_some() {
            return None;
        }
        // Synonyms set while the server runs are part of what a query reads.
        let synonyms = self.synonyms().map_or(0, |s| Arc::as_ptr(&s) as *const u8 as usize as u64);
        let flags = [req.shard as u8, req.own_read as u8, req.boosts_alone as u8];
        Some(
            crate::answers::KeyBuilder::default()
                .key(base)
                .params(&req.params)
                .part(table.as_bytes())
                .part(&flags)
                .part(&synonyms.to_le_bytes())
                .part(&req.stats_version.to_le_bytes())
                .part(&self.max_rows.unwrap_or(0).to_le_bytes())
                .finish(),
        )
    }

    /// [`Self::query`], with its steps.
    async fn query_steps(self: &Arc<Self>, req: QueryReq) -> Result<Answer, ApiError> {
        // alerts agent: Contain rules, at admission ([`crate::contain`]).
        let _turn = self.contained(&req).await?;
        self.dispatch(req).await
    }

    /// [`Self::query`] of a query the Contain rules admitted: a query that
    /// runs as steps runs each step here, under the turn its statement
    /// took, rather than asking for another (a rule that lets one query of
    /// a key run at a time would otherwise refuse the statement's second
    /// step while its first holds the turn).
    async fn dispatch(self: &Arc<Self>, req: QueryReq) -> Result<Answer, ApiError> {
        // A query that asks a language model about its rows
        // ([`chloedb::query::llm`]): the rows first, as any query finds them
        // (a coordinator's from its shards, merged), then the questions,
        // here.
        // A query that packs its rows into a budget of tokens
        // ([`chloedb::query::pack`]): its slices of candidates, and the rows
        // it follows, each a query as any other (a coordinator's merged
        // from its shards), and the choice here.
        if req.plan.is_none() && !req.shard && chloedb::query::pack::may_pack(&req.sql) {
            let catalog = self.catalog();
            let table_of = |t: &str| catalog.iter().find(|s| s.table.eq_ignore_ascii_case(t)).cloned();
            if let Some(mut p) = chloedb::query::pack::Packer::of_sql(&req.sql, &req.params, &table_of)? {
                if req.scroll.is_some() {
                    return Err(ApiError::bad("a query that packs its rows is not scrolled: PACK n TOKENS reads what fills the budget"));
                }
                let t0 = Instant::now();
                let deadline = t0 + self.timeout_of(req.timeout_ms);
                // Every read of one version: on a server, the client's
                // lease, or one held for the pack and released after it
                // (kept for the client with `hold`). A coordinator's shards
                // each read their latest.
                let local = matches!(self.backend, Backend::Local(_));
                let mut lease = req.snapshot;
                let (mut last, mut failed) = (None, None);
                loop {
                    let step: Result<Option<Answer>, ApiError> = async {
                        let Some(next) = p.next_query()? else { return Ok(None) };
                        let mut sub = req.clone();
                        // As text that reads back as the very query.
                        sub.sql = next.sql()?;
                        sub.params = next.params;
                        (sub.snapshot, sub.hold, sub.release) = (lease, local && lease == 0, false);
                        // What is left of the statement's time, not a
                        // deadline of each read's own.
                        sub.timeout_ms = (deadline.saturating_duration_since(Instant::now()).as_millis() as u64).max(1);
                        let mut a = Box::pin(self.dispatch(sub)).await?;
                        p.feed(std::mem::replace(&mut a.rs, no_rows()))?;
                        Ok(Some(a))
                    }
                    .await;
                    match step {
                        Ok(Some(a)) => {
                            if lease == 0 {
                                lease = a.lease;
                            }
                            last = Some(a);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                }
                let read = match (local, lease) {
                    (true, l) if l != 0 => self.leased(l).ok().map(|s| (s.txn(), s.seq_no())),
                    _ => None,
                };
                let made = lease != req.snapshot && lease != 0;
                if (made && (!req.hold || failed.is_some())) || (req.release && req.snapshot != 0) {
                    self.release(&[lease]);
                }
                if let Some(e) = failed {
                    return Err(e);
                }
                let packed = p.finish(read.map(|r| r.0), read.and_then(|r| r.1))?;
                let Some(mut a) = last else { return Err(ApiError::bad("PACK: the query read no rows")) };
                a.rs = packed.rs;
                a.lease = if req.hold { lease } else { 0 };
                a.took_ms = t0.elapsed().as_secs_f64() * 1e3;
                return Ok(a);
            }
        }
        // The chunks of a table's text (`FROM chunks_of(table, field)`,
        // [`chloedb::query::chunks`]): its rows as any query finds them,
        // split here.
        if req.plan.is_none() && !req.shard {
            if let Some(c) = chloedb::query::chunks::of_sql(&req.sql, &req.params)? {
                let schema = self.catalog().into_iter().find(|s| s.table.eq_ignore_ascii_case(&c.table)).ok_or_else(|| ApiError::bad(format!("query error: no table {}", c.table)))?;
                // As many of the table's rows as fill the LIMIT, read again
                // with more when rows without text leave it unfilled.
                let mut rows = c.source_rows();
                loop {
                    let mut source = req.clone();
                    (source.sql, source.scroll, source.hold, source.own_read) = (c.source_sql_of(&schema, rows)?, None, false, true);
                    let mut a = Box::pin(self.dispatch(source)).await?;
                    let t0 = Instant::now();
                    let chunks = c.finish(&a.rs);
                    if let Some(more) = c.more_rows(rows, &a.rs, &chunks) {
                        rows = more;
                        continue;
                    }
                    a.rs = chunks;
                    a.took_ms += t0.elapsed().as_secs_f64() * 1e3;
                    return Ok(a);
                }
            }
        }
        // Understood words (`bm25(.., understand => 'boost')`,
        // [`chloedb::query::understand`]): the table's keyword values the
        // words name become conditions, then the query runs as any does
        // (and may still expand).
        if req.plan.is_none() && !req.shard {
            if let Some(u) = chloedb::query::understand::plan_sql(&req.sql, &req.params)? {
                let t0 = Instant::now();
                let catalog = self.catalog();
                let gaz = self.gazetteers(&u.needs(&catalog)?, Some(&req)).await?;
                let mut made = u.understood(&catalog, &gaz, &req.params)?;
                for m in &mut made {
                    let counts = self.understanding_counts(m, Some(&req)).await?;
                    m.confirm(&counts);
                }
                let mut second = req.clone();
                second.sql = u.rewrite(&made)?.to_string();
                let mut a = Box::pin(self.dispatch(second)).await?;
                a.rs.stats.plan.splice(0..0, made.iter().map(|m| m.line()));
                a.took_ms = t0.elapsed().as_secs_f64() * 1e3;
                return Ok(a);
            }
        }
        // Expanded ranking (`bm25(.., expand => n)`, [`chloedb::query::prf`]):
        // the rows of each expanded call first, as any query finds them (a
        // coordinator's from its shards, merged), then the query rewritten
        // to the terms they hold, which every shard ranks by alike.
        if req.plan.is_none() && !req.shard {
            if let Some(x) = chloedb::query::prf::plan_sql(&req.sql, &req.params)? {
                let t0 = Instant::now();
                let mut rows = Vec::new();
                let catalog = self.catalog();
                let schemas: Vec<&chloedb::Schema> = catalog.iter().map(|s| s.as_ref()).collect();
                for f in x.feedback(&schemas)? {
                    let mut fr = req.clone();
                    (fr.sql, fr.scroll, fr.hold) = (f.to_string(), None, false);
                    rows.push(Box::pin(self.dispatch(fr)).await?.rs);
                }
                let analyzers = x
                    .fields()
                    .iter()
                    .map(|(t, f)| {
                        let schema = catalog.iter().find(|s| s.table.eq_ignore_ascii_case(t)).ok_or_else(|| ApiError::bad(format!("no table {t}")))?;
                        Ok(schema.field(f).ok_or_else(|| ApiError::bad(format!("{t} has no field {f}")))?.text_analyzer())
                    })
                    .collect::<Result<Vec<_>, ApiError>>()?;
                let (rewritten, made) = x.finish(&rows, &analyzers, &req.params)?;
                let mut second = req.clone();
                second.sql = rewritten.to_string();
                let mut a = Box::pin(self.dispatch(second)).await?;
                a.rs.stats.plan.splice(0..0, made.iter().map(|m| m.line()));
                a.took_ms = t0.elapsed().as_secs_f64() * 1e3;
                return Ok(a);
            }
        }
        // `recommend(...)` ([`chloedb::query::recommend`]): the items a
        // recommender ranks, computed on this database, then the query with
        // each call as `ranked(...)` of their keys.
        if req.plan.is_none() && !req.shard && chloedb::query::recommend::may_recommend(&req.sql) {
            if let Some(a) = self.recommend_query(&req).await? {
                return Ok(a);
            }
        }
        if req.plan.is_none() && !req.shard {
            if let Some(mut split) = chloedb::query::llm::split_sql(&req.sql, &self.functions(), &req.params)? {
                // The calls read no more rows than a query answers.
                if let Some(m) = self.max_rows {
                    split.cap_rows(m);
                }
                // Calls of a classifier on a parameter, computed: the query
                // runs as any does.
                if split.is_folded() {
                    let mut folded = req.clone();
                    folded.sql = split.first.to_string();
                    let mut a = self.query_admitted(folded).await?;
                    a.rs.stats.plan.insert(0, split.folded_line());
                    return Ok(a);
                }
                if req.scroll.is_some() {
                    return Err(ApiError::bad("a query that calls llm() is not scrolled: page it with LIMIT and OFFSET"));
                }
                let mut first = req.clone();
                // As text that reads back as the very first step.
                first.sql = split.first_sql()?;
                // It reads one row past what the calls take, to say there
                // are more: the second step answers within --max-rows.
                first.own_read = true;
                let mut a = self.query_admitted(first).await?;
                let rs = std::mem::replace(&mut a.rs, ResultSet { columns: Vec::new(), rows: Vec::new(), keys: Vec::new(), scores: Vec::new(), parts: None, stats: Default::default() });
                let (params, models) = (req.params.clone(), self.models_dir());
                let t0 = Instant::now();
                // Each batch of questions asked takes from the caller's
                // model budget, where it is sent.
                let (budget, who) = (self.model_budget.clone(), req.who.clone());
                let refused: std::sync::Mutex<Option<ApiError>> = std::sync::Mutex::new(None);
                let finished = tokio::task::spawn_blocking(move || {
                    let take = |n: u64| {
                        budget.take(who.as_ref(), n, "llm()").map_err(|e| {
                            let m = e.message.clone();
                            *refused.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                            chloedb::Error::Unavailable(m)
                        })
                    };
                    let r = split.finish_within(rs, &params, &models, Some(&take));
                    (r, refused.into_inner().unwrap_or_else(|p| p.into_inner()))
                })
                .await
                .map_err(|e| ApiError::internal(format!("llm(): {e}")))?;
                a.rs = match finished {
                    (_, Some(refused)) => return Err(refused),
                    (r, None) => r?,
                };
                a.took_ms += t0.elapsed().as_secs_f64() * 1e3;
                return Ok(a);
            }
        }
        self.query_admitted(req).await
    }

    /// A query that calls `recommend(...)`, or `None` for one that calls
    /// none. A coordinator refuses it: each shard holds the counts of the
    /// events it holds, not a user's whole history.
    async fn recommend_query(self: &Arc<Self>, req: &QueryReq) -> Result<Option<Answer>, ApiError> {
        let t0 = Instant::now();
        let db = match &self.backend {
            Backend::Local(db) => db.clone(),
            Backend::Cluster(_) => {
                if !chloedb::query::recommend::calls_sql(&req.sql, &req.params)? {
                    return Ok(None);
                }
                return Err(ApiError::bad("recommend() does not run through a coordinator: a recommender counts the events of one database (run it on a server that holds the events table, or keep the counts in tables of your own; see cluster-limits)"));
            }
        };
        if req.scroll.is_some() {
            return Err(ApiError::bad("a query that calls recommend() is not scrolled: page it with LIMIT and OFFSET"));
        }
        let (sql, params) = (req.sql.clone(), req.params.clone());
        let done = tokio::task::spawn_blocking(move || db.recommend_sql(&sql, &params)).await.map_err(ApiError::internal)??;
        match done {
            None => Ok(None),
            Some(chloedb::db::RecommendSql::Rows(rs)) => Ok(Some(Answer { rs, took_ms: t0.elapsed().as_secs_f64() * 1e3, kind: "recommend", missing: Vec::new(), lease: 0, arrow: Vec::new(), stale: false, cursor: None })),
            Some(chloedb::db::RecommendSql::Query { sql, lines }) => {
                let mut second = req.clone();
                second.sql = sql;
                let mut a = Box::pin(self.query(second)).await?;
                a.rs.stats.plan.splice(0..0, lines);
                a.took_ms = t0.elapsed().as_secs_f64() * 1e3;
                Ok(Some(a))
            }
        }
    }

    /// [`Self::query`] of a query admitted, which calls no language model.
    async fn query_admitted(self: &Arc<Self>, req: QueryReq) -> Result<Answer, ApiError> {
        let again = (matches!(self.backend, Backend::Cluster(_)) && req.plan.is_none() && req.sql.to_ascii_lowercase().contains("rank_profile")).then(|| req.clone());
        match (self.query_once(req).await, again) {
            (Err(e), Some(req)) if profile::is_stale(&e.message) => {
                self.learn_tables(true).await;
                self.query_once(req).await
            }
            (r, _) => r,
        }
    }

    async fn query_once(self: &Arc<Self>, req: QueryReq) -> Result<Answer, ApiError> {
        // A statement that nests deep is bound, planned, run and dropped
        // on a thread with the stack for it, a worker's 2 MiB being too
        // little for some (chloedb::query::parse::deep_stack).
        if chloedb::query::parse::nests_deep(&req.sql) && !chloedb::query::parse::on_deep_stack() {
            let (me, rt) = (self.clone(), tokio::runtime::Handle::current());
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("chloedb-deep".into())
                .stack_size(chloedb::query::parse::QUERY_STACK)
                .spawn(move || {
                    chloedb::query::parse::mark_deep_stack();
                    let _ = tx.send(rt.block_on(me.query_once_here(req)));
                })
                .map_err(ApiError::internal)?;
            return rx.await.map_err(ApiError::internal)?;
        }
        self.query_once_here(req).await
    }

    /// [`Self::query_once`] on this thread.
    async fn query_once_here(self: &Arc<Self>, mut req: QueryReq) -> Result<Answer, ApiError> {
        let t0 = Instant::now();
        // A JSON field's paths listing: from this database's terms, or a
        // coordinator's shards' listings summed.
        if req.plan.is_none() {
            if let Some(call) = Call::of_sql(&req.sql)? {
                let res = self.json_paths(&call, &req.params, &req.after, req.timeout_ms).await.map(|p| (p.to_result(), Vec::new()));
                let meta = self.meta(&req);
                return self.done(&req.sql, "paths", t0, None, req.shard, (false, req.quiet), &meta, res);
            }
        }
        let timeout = self.timeout_of(req.timeout_ms);
        // Bound against every table: a query of one table is the
        // executor's, any other a compound plan.
        let tp = Instant::now();
        let plan = match &req.plan {
            Some(p) => Ok(p.clone()),
            None => self.bound_read(&req.sql, &req.params, timeout).await.map(Arc::new),
        };
        // A plan made with a rank profile runs only on a database whose
        // profile of that name is the same.
        let plan = match (plan, &self.backend) {
            (Ok(p), Backend::Local(_)) if p.root.has_profiles() => profile::check_plan(&p, &|t, n| self.rank_profile(t, n).map(|p| p.hash())).map(|_| p).map_err(ApiError::from),
            (p, _) => p,
        };
        let plan = plan.and_then(|p| self.open_ranking_models(p, req.shard));
        telemetry::record("plan", tp, if plan.is_ok() { "ok" } else { "error" });
        let bound = match plan {
            Ok(plan) if plan.root.uses_operators() => {
                // A compound query runs as its plan; a coordinator sends the
                // shards the plan's leaves.
                req.plan = Some(plan.clone());
                return self.compound(req, plan, t0, timeout).await;
            }
            Ok(plan) => match self.readable_table(plan.table()).await {
                Ok(schema) => {
                    // A coordinator sends the shards a plan by a rank
                    // profile or a model as is, naming the profile or the
                    // model's hash they check.
                    if plan.root.has_profiles() || plan.root.has_models() {
                        req.plan = Some(plan.clone());
                    }
                    plan.to_select(&schema).map(|select| (schema, select)).map_err(ApiError::from)
                }
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        match bound {
            Ok((schema, select)) => self.select(req, schema, select, t0, timeout).await,
            Err(e) => {
                let secs = t0.elapsed().as_secs_f64();
                // A client's deadline that passed while the query was
                // planned is the client's, as one that passes later is
                // (a slow runner planned past a 1 ms deadline on 11-14).
                let meta = self.meta(&req);
                let outcome = telemetry::outcome_of(e.status, meta.client_deadline);
                self.metrics.query("invalid", outcome, secs);
                // Kept with its error, as a query that ran and failed is.
                self.query_log.add(&req.sql, "invalid", outcome, secs, 0, req.shard, Some(&e.message), false, None, false, &meta);
                Err(e)
            }
        }
    }

    /// Runs a query and logs what it served ([`chloedb::rank_log`]): the
    /// answer has the query's columns, and the log's rows are written as
    /// [`Self::write_ops`] writes them, through a coordinator to the shards
    /// their keys place them on. The first query logged to a table makes
    /// it, on every shard through a coordinator. A replica refuses it, the
    /// log being a write. Returns the request's id and the log's write.
    /// A logged query needs a write key, and the first one to a table,
    /// which creates it, an admin key. An interleaved one
    /// ([`Log::interleave`]) runs both rankings at once and answers their
    /// [`rank_log::interleave`], with each row's team.
    pub async fn query_logged(self: &Arc<Self>, who: &Principal, mut req: QueryReq, log: Log) -> Result<(Answer, rank_log::Served, Written), ApiError> {
        // A key limited to tables: write on the log's table, and read on
        // every table the query (and an interleaved one) reads.
        match who.limited() {
            true => {
                who.may_table(Scope::Write, &log.table, "a logged query (it writes the rank log)")?;
                let (names, functions) = (self.table_names(), self.functions());
                crate::scoped::check_reads(who, &req.sql, &names, &functions)?;
                if let Some(il) = &log.interleave {
                    crate::scoped::check_reads(who, &il.sql, &names, &functions)?;
                }
            }
            false => who.may(Scope::Write, "a logged query (it writes the rank log)")?,
        }
        self.takes_writes()?;
        let schema = match self.table(&log.table).await {
            Ok(s) => s,
            Err(_) => {
                let what = format!("the first query logged to {} (it creates the table; log once with an admin key, or create it with CREATE TABLE)", log.table);
                match who.limited() {
                    true => who.may_table(Scope::Admin, &log.table, &what)?,
                    false => who.may(Scope::Admin, &what)?,
                }
                self.sql(who, &rank_log::create_sql(&log.table)?, &Params::new(), After::Any, req.timeout_ms).await?;
                self.table(&log.table).await?
            }
        };
        rank_log::check(&schema)?;
        let p = rank_log::prepare(&req.sql, &req.params, &log)?;
        let take = |a: &mut Answer| std::mem::replace(&mut a.rs, ResultSet { columns: Vec::new(), rows: Vec::new(), keys: Vec::new(), scores: Vec::new(), parts: None, stats: Default::default() });
        let (mut a, mut served) = match &log.interleave {
            None => {
                req.sql = p.sql.clone();
                let mut a = self.query(req).await?;
                let served = rank_log::serve(take(&mut a), &p, &log)?;
                (a, served)
            }
            Some(il) => {
                let pb = rank_log::prepare(&il.sql, &req.params, &log)?;
                let mut rb = req.clone();
                req.sql = p.sql.clone();
                rb.sql = pb.sql.clone();
                let (mut a, mut b) = tokio::try_join!(self.query(req), self.query(rb))?;
                let (sa, sb) = (rank_log::serve(take(&mut a), &p, &log)?, rank_log::serve(take(&mut b), &pb, &log)?);
                for m in b.missing {
                    if !a.missing.contains(&m) {
                        a.missing.push(m);
                    }
                }
                (a, rank_log::interleave(sa, sb, &p, il)?)
            }
        };
        rank_log::keep_query(&mut served, &p, &schema);
        a.rs = std::mem::replace(&mut served.rs, ResultSet { columns: Vec::new(), rows: Vec::new(), keys: Vec::new(), scores: Vec::new(), parts: None, stats: Default::default() });
        let ops: Vec<Json> = served.entries.iter().map(|e| json!({"op": "upsert", "table": log.table, "doc": e.to_json()})).collect();
        let w = match ops.is_empty() {
            true => Written::default(),
            false => self.write_ops(ops, None).await?,
        };
        Ok((a, served, w))
    }

    /// Runs a plan ([`chloedb::query::ir`]): a read answers as
    /// [`Service::query`] does, and a write as [`Service::sql`] does (on any
    /// table). A coordinator takes writes as SQL until its shards take write
    /// plans. A read sees the writes `after` names.
    pub async fn run_plan(self: &Arc<Self>, who: &Principal, plan: Plan, after: After, timeout_ms: u64) -> Result<SqlAnswer, ApiError> {
        crate::scoped::check_plan(who, &plan)?;
        if plan.writes() {
            who.may(Scope::Write, "a write plan")?;
            if !plan.is_write() {
                return Err(ApiError::bad("a plan writes at its root only: an INSERT, UPDATE or DELETE inside a read is not run"));
            }
        }
        let t0 = Instant::now();
        // What the slow-query log shows of the plan.
        let catalog = self.catalog();
        let text = plan.explain(&catalog.iter().map(|t| &**t).collect::<Vec<_>>()).join(" / ");
        if plan.is_write() {
            if after != After::Any {
                return Err(ApiError::bad(FOR_READS));
            }
            let Backend::Local(db) = &self.backend else {
                return Err(ApiError::bad("a coordinator takes writes as SQL; its shards do not take write plans yet"));
            };
            let w = self.write_local(db, move |db| db.prepare_plan(&plan).map(Some), timeout_ms, None).await;
            let w = self.wrote(w, t0, Some(&text))?;
            return Ok(SqlAnswer::Written { w, took_ms: t0.elapsed().as_secs_f64() * 1e3 });
        }
        let req = QueryReq { sql: text, timeout_ms, plan: Some(Arc::new(plan)), after, ..Default::default() };
        self.query(req).await.map(SqlAnswer::Rows)
    }

    /// Runs a bound query of table `schema`, on the database or over the
    /// shards.
    async fn select(self: &Arc<Self>, mut req: QueryReq, schema: Arc<Schema>, select: Select, t0: Instant, timeout: Duration) -> Result<Answer, ApiError> {
        let kind = metrics::kind(&select);
        // A client's answer counts its rows' documents as retrieved; a
        // shard's to a coordinator, which counts them, does not.
        req.reads = !req.shard && !req.quiet && self.reads.on();
        let mut served = Vec::new();
        let mut lease = 0;
        // The statistics the query was sent are not of the version it reads.
        let mut stale = false;
        req.profile |= self.profiling.cpu_all.load(Ordering::Relaxed);
        let profiled = req.profile;
        let meta = self.meta(&req);
        let live = self.live.start(&req.sql, kind, req.shard, &meta);
        // A shard's answer to a coordinator's leaf, as Arrow columns.
        let mut arrow = Vec::new();
        // A scroll's cursor, while rows are left.
        let mut cursor = None;
        let res = match &self.backend {
            Backend::Local(db) => {
                let stats = req.stats.take().map(Arc::new);
                // A shard's scroll scores its later pages as its first.
                let scroll_stats = stats.clone().filter(|_| req.shard && req.scroll.is_some());
                LiveQueries::run(&live, async {
                    // A read on a replica may wait for a write it must see,
                    // before it takes a query slot.
                    let tw = Instant::now();
                    let snap = self.read_snapshot(db, req.snapshot, &req.after, t0 + timeout).await?;
                    if req.after != After::Any {
                        telemetry::record("wait for writes", tw, "ok");
                    }
                    if req.stats_version != 0 && snap.table(&schema.table).is_some_and(|t| req.stats_version != self.stats_version(&t)) {
                        stale = true;
                        return Ok((no_rows(), Vec::new()));
                    }
                    // An answer kept for the same query on this version
                    // ([`crate::answers`]) needs no slot and no executor.
                    let txn = snap.txn();
                    let cached = self.answer_key(&req, &schema.table);
                    if let (Some(k), Some(cache)) = (cached, &self.answers) {
                        let tc = Instant::now();
                        if let Some(kept) = cache.get(k, txn) {
                            let mut rs = (*kept).clone();
                            rs.stats.steps.push(chloedb::query::steps::Step {
                                op: "answer cache".into(),
                                detail: format!("the answer of this query on version {txn}"),
                                start_us: steps::us_at(tc),
                                us: steps::tenth(tc.elapsed().as_nanos() as f64 / 1e3),
                                rows: Some(rs.rows.len() as u64),
                                ..Default::default()
                            });
                            telemetry::record("answer cache", tc, "hit");
                            return Ok((rs, Vec::new()));
                        }
                    }
                    let ta = Instant::now();
                    let permit = self.admit(self.admission_budget(timeout, req.shard)).await.ok_or_else(too_many);
                    telemetry::record("admission", ta, if permit.is_ok() { "ok" } else { "rejected" });
                    let permit = permit?;
                    lease = self.held(&req, &snap, timeout)?;
                    let view = snap.table(&schema.table).ok_or_else(|| ApiError::bad(format!("query error: no such table: {}", schema.table)))?;
                    let (running, n) = Running::start(&self.running);
                    let cancel = Cancel::default();
                    // Each running query gets its share of the cores for its
                    // segments (as ChloeCtx's recall does).
                    let opts = ExecOptions {
                        stats,
                        // A query round on the lease of a statistics round
                        // reuses its typo expansions.
                        expansions: self.lease_expansions(req.snapshot),
                        shard: req.shard,
                        threads: (self.cores / n).max(1),
                        deadline: Some(t0 + timeout),
                        cancel: Some(cancel.0.clone()),
                        synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()),
                        // A coordinator's compound queries ask a shard for
                        // as many rows as a compound query's leaf may hold.
                        max_rows: if req.own_read { None } else if req.shard { self.max_rows.map(|m| m.max(compound::MAX_ROWS + 1)) } else { self.max_rows },
                        max_groups: self.max_groups,
                        // The server's own read is held to a budget of its
                        // own rather than to --max-rows.
                        memory: if req.own_read { chloedb::query::memory::own_read_budget(self.query_memory.as_ref()) } else { self.query_memory.clone() },
                        profile: req.profile,
                        boost_inputs: db.exec.boost_inputs && !req.boosts_alone,
                        ..db.exec.clone()
                    };
                    let db = db.clone();
                    // Arrow columns for a coordinator that puts this
                    // shard's rows after the others'.
                    let columns = req.arrow.filter(|_| req.shard && cfg!(feature = "datafusion") && chloedb::cluster::concatenates(&select));
                    // A scroll's first page, at most the window, and what
                    // its ordered rows may hold.
                    let scroll = req.scroll.as_ref().map(|a| (self.page_size(a.page), self.max_scroll_rows));
                    // The work holds the slot and the count until it ends,
                    // and stops at its next check if this future is dropped.
                    let te = Instant::now();
                    let deep = chloedb::query::parse::nests_deep(&req.sql);
                    let r = tokio::task::spawn_blocking(move || {
                        let _held = (permit, running);
                        let started = Instant::now();
                        // A statement that nests deep runs on a thread with the stack for it.
                        let r = chloedb::query::parse::deep_stack(deep, || match (scroll, columns) {
                            (Some((page, hold)), _) => db
                                .scroll(&view, &select, &opts, page, hold)
                                .map(|(rs, s)| (rs, Vec::new(), Some(s)))
                                // What a scroll may hold is this server's flag.
                                .map_err(|e| match e {
                                    chloedb::Error::Query(m) if m.contains("a scroll of") => chloedb::Error::Query(format!("{m} (--max-scroll-rows)")),
                                    e => e,
                                }),
                            (None, Some(over)) => db.execute_leaf(&view, &select, &opts, over).and_then(shard_leaf).map(|(rs, ipc)| (rs, ipc, None)),
                            (None, None) => db.execute(&view, &select, &opts).map(|rs| (rs, Vec::new(), None)),
                        });
                        (r, started, Instant::now())
                    })
                    .await;
                    let back = Instant::now();
                    drop(cancel);
                    telemetry::record("execute", te, if matches!(r, Ok((Ok(_), ..))) { "ok" } else { "error" });
                    match r {
                        Ok((r, started, ended)) => {
                            let (mut rs, ipc, scrolled) = r?;
                            steps::around(&mut rs.stats.steps, te, started, ended, back);
                            if let (Some(k), Some(cache), true, None) = (cached, &self.answers, ipc.is_empty(), &scrolled) {
                                cache.put(k, txn, &rs);
                            }
                            arrow = ipc;
                            if let (Some(s), Some(ask)) = (scrolled, &req.scroll) {
                                cursor = self.keep_scroll(scroll::Paged::Db(s), Some(snap.clone()), &schema.table, &req.sql, ask, req.shard, scroll_stats)?;
                            }
                            Ok((rs, Vec::new()))
                        }
                        Err(e) => Err(ApiError::internal(e)),
                    }
                })
                .await
            }
            Backend::Cluster(c) => {
                LiveQueries::run(&live, async {
                    let ta = Instant::now();
                    let permit = self.admit(self.admission_budget(timeout, false)).await;
                    telemetry::record("admission", ta, if permit.is_some() { "ok" } else { "rejected" });
                    let permit = permit.ok_or_else(too_many)?;
                    let reach = select.limit.unwrap_or(0).saturating_add(select.offset);
                    let r = match (&req.scroll, self.max_rows.filter(|&m| reach > m && !req.own_read)) {
                        // The shards' scrolls, their pages merged as they come.
                        (Some(ask), _) if chloedb::cluster::streams(&select) => {
                            let keep_alive = self.keep_alive(ask.keep_alive, false) + crate::coord::SHARD_KEEP_ALIVE_MARGIN;
                            match c.scroll(&req, &schema, &select, self.page_size(ask.page), keep_alive, t0 + timeout).await {
                                Ok((rs, s)) => {
                                    if let Some(s) = s {
                                        cursor = self.keep_scroll(scroll::Paged::Shards(Box::new(s)), None, &schema.table, &req.sql, ask, false, None)?;
                                    }
                                    Ok((rs, Vec::new()))
                                }
                                Err(e) => Err(e),
                            }
                        }
                        // Groups, counts, fused or reranked rows: answered
                        // whole, and held.
                        (Some(ask), _) => {
                            let most = self.max_scroll_rows;
                            match c.query(&req, &schema, &select, t0 + timeout).await {
                                Ok(m) if m.rs.rows.len() > most => Err(ApiError::bad(format!("query error: the query answers {} rows; a scroll of groups, counts, fused or reranked rows over shards holds them, at most {most} (--max-scroll-rows)", m.rs.rows.len()))),
                                Ok(m) => {
                                    let (first, s) = chloedb::query::exec::Scroll::of_rows(m.rs, self.page_size(ask.page));
                                    cursor = self.keep_scroll(scroll::Paged::Db(s), None, &schema.table, &req.sql, ask, false, None)?;
                                    Ok((first, m.missing))
                                }
                                Err(e) => Err(e),
                            }
                        }
                        (None, Some(m)) => Err(ApiError::bad(format!("query error: LIMIT and OFFSET reach row {reach}; a query returns at most {m} rows"))),
                        (None, None) => c.query(&req, &schema, &select, t0 + timeout).await.map(|m| {
                            served = m.served;
                            (m.rs, m.missing)
                        }),
                    };
                    drop(permit);
                    r
                })
                .await
            }
        };
        drop(live);
        // Not a query that ran: the coordinator sends it again with the
        // statistics of this version.
        if stale {
            self.stale_stats.fetch_add(1, Ordering::Relaxed);
            return Ok(Answer { rs: no_rows(), took_ms: t0.elapsed().as_secs_f64() * 1e3, kind, missing: Vec::new(), lease: 0, arrow: Vec::new(), stale: true, cursor: None });
        }
        if req.reads {
            match (&res, &self.backend) {
                (Ok((rs, _)), Backend::Local(_)) => self.reads.retrieved(&schema.table, crate::reads::At::here(), &rs.keys),
                (Ok(_), Backend::Cluster(_)) => {
                    for s in served {
                        self.reads.retrieved(&schema.table, s.at, &s.keys);
                    }
                }
                _ => {}
            }
        }
        // A shard's answer to a coordinator is not a client's.
        let a = self.done(&req.sql, kind, t0, self.max_rows.filter(|_| !req.shard && !req.own_read), req.shard, (profiled, req.quiet), &meta, res);
        // A lease made for a query that failed holds nothing to fetch.
        let made = (lease != req.snapshot).then_some(lease).filter(|_| a.is_err());
        if let Some(l) = (req.release && req.snapshot != 0).then_some(req.snapshot).or(made) {
            self.release(&[l]);
        }
        a.map(|a| Answer { lease, arrow, cursor, ..a })
    }

    /// What the query log keeps of a query besides its text: who sent it
    /// ([`telemetry::caller`]), its parameters, its deadline, and whether
    /// that is the client's own.
    pub fn meta(&self, req: &QueryReq) -> telemetry::Meta {
        let deadline = self.timeout_of(req.timeout_ms);
        telemetry::Meta {
            caller: telemetry::caller(),
            params: telemetry::params_json(&req.params),
            deadline_ms: deadline.as_millis() as u64,
            client_deadline: (!req.shard || part_of_clients_deadline()) && req.timeout_ms > 0 && deadline < self.timeout,
        }
    }

    /// Counts a query's outcome, logs it if it was slow, and makes its
    /// answer. One with more rows than `window` (a compound query's, or a
    /// coordinator's without a LIMIT) fails.
    #[allow(clippy::too_many_arguments)]
    fn done(&self, sql: &str, kind: &'static str, t0: Instant, window: Option<usize>, shard: bool, (profiled, quiet): (bool, bool), meta: &telemetry::Meta, res: Result<(ResultSet, Vec<String>), ApiError>) -> Result<Answer, ApiError> {
        let mut res = match (res, window) {
            (Ok((rs, _)), Some(m)) if rs.rows.len() > m => Err(ApiError::bad(format!("query error: the query matches more than {m} rows, the most a query returns; add a LIMIT"))),
            (r, _) => r,
        };
        // Past the window, a server's client can scroll.
        if let Err(e) = &mut res {
            let window = e.message.contains("a query returns at most") || e.message.contains("the most a query returns");
            if window && !shard {
                e.message.push_str(" (or read every row a page at a time: a scroll, page_size)");
            }
        }
        // The steps count from the request's start, as its trace does: a
        // shard's, from the start of the call the coordinator made.
        if let Ok((rs, _)) = &mut res {
            let start = telemetry::trace().map_or(t0, |t| t.t0());
            steps::rebase(&mut rs.stats.steps, steps::us_at(start));
        }
        let secs = t0.elapsed().as_secs_f64();
        // Out of query memory (429), or a shard was; a client's own
        // deadline that passed is not the server's failure.
        let outcome = match &res {
            Ok((_, missing)) if !missing.is_empty() => "partial",
            Ok(_) => "ok",
            Err(e) => telemetry::outcome_of(e.status, meta.client_deadline),
        };
        self.metrics.query(kind, outcome, secs);
        let rows = res.as_ref().map_or(0, |(rs, _)| rs.rows.len());
        let slow = self.slow_after().is_some_and(|s| t0.elapsed() >= s);
        if slow {
            self.metrics.slow("query");
            tracing::warn!(kind, outcome, secs, rows, sql = %obs::truncate(sql, 2000), "slow query");
        }
        if !quiet {
            self.query_log.add(sql, kind, outcome, secs, rows, shard, res.as_ref().err().map(|e| e.message.as_str()), slow, res.as_ref().ok().map(|(rs, _)| &rs.stats), profiled, meta);
        }
        res.map(|(rs, missing)| Answer { rs, took_ms: secs * 1e3, kind, missing, lease: 0, arrow: Vec::new(), stale: false, cursor: None })
    }

    /// A compound query ([`compound`]): its plan's leaves run on one
    /// snapshot here, or each through the coordinator, and its operators
    /// here.
    async fn compound(self: &Arc<Self>, mut req: QueryReq, plan: Arc<Plan>, t0: Instant, timeout: Duration) -> Result<Answer, ApiError> {
        let kind = "compound";
        req.profile |= self.profiling.cpu_all.load(Ordering::Relaxed);
        let (profiled, quiet) = (req.profile, req.quiet);
        let catalog = self.readable();
        let deadline = t0 + timeout;
        let req_sql = req.sql.clone();
        let meta = self.meta(&req);
        let scroll = req.scroll.clone();
        let live = self.live.start(&req.sql, kind, req.shard, &meta);
        let r = LiveQueries::run(&live, self.compound_run(req, plan, catalog, t0, timeout)).await;
        drop(live);
        let res = match r {
            Err(_) if Instant::now() > deadline => Err(ApiError { code: None, status: 504, message: format!("the query ran past its {} ms deadline", timeout.as_millis()) }),
            r => r,
        };
        let Some(ask) = scroll else {
            return self.done(&req_sql, kind, t0, self.max_rows, false, (profiled, quiet), &meta, res);
        };
        // A scroll of a compound query holds its answer's rows.
        let most = self.max_scroll_rows;
        let res = match res {
            Ok((rs, _)) if rs.rows.len() > most => Err(ApiError::bad(format!(
                "query error: the query answers {} rows; a scroll of a join, WITH, UNION or DISTINCT holds its answer's rows, at most {most} (--max-scroll-rows)",
                rs.rows.len()
            ))),
            r => r,
        };
        let a = self.done(&req_sql, kind, t0, None, false, (profiled, quiet), &meta, res)?;
        let (first, s) = chloedb::query::exec::Scroll::of_rows(a.rs, self.page_size(ask.page));
        let cursor = self.keep_scroll(scroll::Paged::Db(s), None, "", &req_sql, &ask, false, None)?;
        Ok(Answer { rs: first, cursor, ..a })
    }

    /// The work of [`Self::compound`]; [`Self::done`] counts it.
    async fn compound_run(self: &Arc<Self>, req: QueryReq, plan: Arc<Plan>, catalog: Vec<Arc<Schema>>, t0: Instant, timeout: Duration) -> Result<(ResultSet, Vec<String>), ApiError> {
        let deadline = t0 + timeout;
        // Each leaf checks it: the caller may have stopped waiting.
        let cancel = Cancel::default();
        let r = match &self.backend {
            Backend::Local(db) => {
                // A read on a replica may wait for a write it must see,
                // before it takes a query slot.
                let snap = self.read_snapshot(db, req.snapshot, &req.after, deadline).await?;
                let Some(permit) = self.admit(timeout).await else { return Err(too_many()) };
                let (running, n) = Running::start(&self.running);
                // Leaves hold up to compound::MAX_ROWS rows; the answer is
                // held to the window below.
                let opts = ExecOptions {
                    threads: (self.cores / n).max(1),
                    deadline: Some(deadline),
                    cancel: Some(cancel.0.clone()),
                    synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()),
                    max_groups: self.max_groups,
                    memory: self.query_memory.clone(),
                    profile: req.profile,
                    engine: self.engine,
                    ..db.exec.clone()
                };
                let db = db.clone();
                let te = Instant::now();
                let deep = chloedb::query::parse::nests_deep(&req.sql);
                let r = tokio::task::spawn_blocking(move || {
                    let _held = (permit, running);
                    let started = Instant::now();
                    // A statement that nests deep runs on a thread with the stack for it.
                    let r = chloedb::query::parse::deep_stack(deep, || {
                        let tables: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
                        ops::run_on_engine(
                            &plan,
                            &tables,
                            &mut |leaf, over| {
                                let view = snap.table(leaf.table()).ok_or_else(|| chloedb::Error::Query(format!("no such table: {}", leaf.table())))?;
                                db.execute_leaf(&view, &leaf.to_select(view.schema())?, &opts, over)
                            },
                            &opts,
                        )
                    });
                    (r, started, Instant::now())
                })
                .await;
                let back = Instant::now();
                r.map(|(r, started, ended)| {
                    r.map(|mut rs| {
                        steps::around(&mut rs.stats.steps, te, started, ended, back);
                        (rs, Vec::new())
                    })
                })
            }
            Backend::Cluster(c) => {
                let Some(permit) = self.admit(timeout).await else { return Err(too_many()) };
                let c = c.clone();
                let rt = tokio::runtime::Handle::current();
                let stop = cancel.0.clone();
                let memory = self.query_memory.clone();
                let (engine, threads) = (self.engine, self.cores.max(1));
                // The leaves' calls to the shards carry the request's id.
                let id = obs::current();
                let trace = telemetry::trace();
                let deep = chloedb::query::parse::nests_deep(&req.sql);
                tokio::task::spawn_blocking(move || chloedb::query::parse::deep_stack(deep, || {
                    let _held = permit;
                    // Shards any leaf was answered without (partial answers).
                    let mut missing: Vec<String> = Vec::new();
                    // The shards run the leaves; DataFusion or the operators
                    // run the rest here, on this server's cores.
                    let checks = ExecOptions { deadline: Some(deadline), cancel: Some(stop.clone()), memory, profile: req.profile, engine, threads, ..ExecOptions::default() };
                    let tables: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
                    let rs = ops::run_on_engine(&plan, &tables, &mut |leaf, over| {
                        checks.check()?;
                        let schema = catalog.iter().find(|t| t.table.eq_ignore_ascii_case(leaf.table())).ok_or_else(|| chloedb::Error::Query(format!("no such table: {}", leaf.table())))?;
                        let select = leaf.to_select(schema)?;
                        // The shards run the leaf's plan; its text names it in their logs.
                        let sql = leaf.explain(&tables).join(" / ");
                        // DataFusion takes Arrow columns past `over` rows.
                        let arrow = (over != usize::MAX).then_some(over);
                        let step = QueryReq { sql, plan: Some(Arc::new(leaf.clone())), partial: req.partial, after: req.after.clone(), profile: req.profile, arrow, ..Default::default() };
                        let mut m = rt.block_on(telemetry::within(trace.clone(), obs::scope(id.clone(), c.query(&step, schema, &select, deadline)))).map_err(|e| chloedb::Error::Query(e.message))?;
                        for s in std::mem::take(&mut m.missing) {
                            if !missing.contains(&s) {
                                missing.push(s);
                            }
                        }
                        Ok(m.leaf())
                    }, &checks);
                    rs.map(|rs| (rs, missing))
                }))
                .await
            }
        };
        drop(cancel);
        match r {
            Ok(r) => r.map_err(ApiError::from),
            Err(e) => Err(ApiError::internal(e)),
        }
    }

    /// Counts a query refused for want of a slot, and says so (429).
    /// Admits a client's query by the Contain rules ([`crate::contain`]),
    /// or refuses it (429 naming the rule), counted and kept as refused.
    /// A coordinator's call as a shard was admitted by the coordinator.
    async fn contained(self: &Arc<Self>, req: &QueryReq) -> Result<Option<crate::contain::Admitted>, ApiError> {
        if req.shard || self.contain.is_empty() {
            return Ok(None);
        }
        let t0 = Instant::now();
        let caller = telemetry::caller();
        let (key, key_id) = (caller.as_ref().and_then(|c| c.key.as_deref()), caller.as_ref().and_then(|c| c.key_id.as_deref()));
        match self.contain.admit(&telemetry::fingerprint(&req.sql), key, key_id, self.timeout_of(req.timeout_ms)).await {
            Ok(a) => Ok(Some(a)),
            Err(e) => {
                let secs = t0.elapsed().as_secs_f64();
                self.metrics.query("contained", "refused", secs);
                self.query_log.add(&req.sql, "contained", "refused", secs, 0, false, Some(&e.message), false, None, false, &self.meta(req));
                Err(e)
            }
        }
    }

    fn rejected(&self, kind: &'static str, t0: Instant) -> ApiError {
        self.metrics.query(kind, "refused", t0.elapsed().as_secs_f64());
        too_many()
    }

    /// How long a query with deadline `timeout` may wait for a slot: until
    /// its deadline, or [`Self::admission_wait`] for a client's query.
    fn admission_budget(&self, timeout: Duration, shard: bool) -> Duration {
        match self.admission_wait {
            Some(w) if !shard => w.min(timeout),
            _ => timeout,
        }
    }

    /// A query slot, waited for until `timeout`; `None` (counted as
    /// rejected) if none came free.
    async fn admit(&self, timeout: Duration) -> Option<OwnedSemaphorePermit> {
        let t0 = Instant::now();
        match tokio::time::timeout(timeout, self.admission.clone().acquire_owned()).await {
            Ok(Ok(p)) => {
                self.metrics.admitted(t0.elapsed().as_secs_f64());
                Some(p)
            }
            _ => {
                self.metrics.rejected();
                None
            }
        }
    }

    /// The slow threshold now: as set while running, else as started.
    pub fn slow_after(&self) -> Option<Duration> {
        self.profiling.slow(self.slow)
    }

    /// Queries running now, including those whose callers stopped waiting
    /// and which have not reached a check yet.
    pub fn running(&self) -> usize {
        self.running.load(Ordering::Relaxed)
    }

    /// The statistics `sql` scores with, a lease on the snapshot they
    /// came from (0 from a coordinator, which sums its shards'), and the
    /// snapshot's statistics version ([`Self::stats_version`]; 0 from a
    /// coordinator). The lease lasts for the query round that follows: the
    /// request's deadline (`timeout_ms`, or the default) and a second. The
    /// snapshot holds the writes `after` names ([`After`]).
    pub async fn term_stats(self: &Arc<Self>, sql: &str, plan: Option<&Plan>, params: &Params, after: &After, timeout_ms: u64) -> Result<(TermStats, u64, u64), ApiError> {
        let bound = match plan {
            Some(p) => p.clone(),
            None => self.bound_read(sql, params, self.timeout_of(timeout_ms)).await?,
        };
        // Statistics are a shard's: its coordinator scores with the
        // cross-encoders.
        let bound = Arc::unwrap_or_clone(self.open_ranking_models(Arc::new(bound), true)?);
        if bound.root.uses_operators() {
            return Err(ApiError::bad("term statistics are a single-table query's"));
        }
        let schema = self.table(bound.table()).await?;
        let select = bound.to_select(&schema)?;
        let timeout = self.timeout_of(timeout_ms);
        match &self.backend {
            Backend::Local(db) => {
                let snap = self.read_snapshot(db, 0, after, Instant::now() + timeout).await?;
                let lease = self.lease_of(snap.clone(), self.lease_ttl.min(timeout + Duration::from_secs(1)))?;
                let view = snap.table(&schema.table).ok_or_else(|| ApiError::bad(format!("query error: no such table: {}", schema.table)))?;
                let version = self.stats_version(&view);
                let db = db.clone();
                // The query round on the lease finds the typo expansions
                // these statistics were found with.
                let opts = ExecOptions { synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()), expansions: self.lease_expansions(lease), ..db.exec.clone() };
                let stats = tokio::task::spawn_blocking(move || db.term_stats_with(&view, &select, &opts)).await.map_err(ApiError::internal)??;
                Ok((stats, lease, version))
            }
            Backend::Cluster(c) => {
                // A query by a rank profile reaches the shards written
                // out, and one by a model with the model's hash.
                let given = plan.filter(|_| !bound.root.has_models()).or((bound.root.has_profiles() || bound.root.has_models()).then_some(&bound));
                Ok((c.term_stats(sql, &schema, given, params, after, Instant::now() + timeout).await?.0, 0, 0))
            }
        }
    }

    /// The loosest level of the guard of `sql` (`min_rows`, `max_rows`)
    /// and how many rows each level from it up to the strictest matches,
    /// summed over the shards of a coordinator, as the query would read
    /// them seeing the writes `after` names.
    pub async fn guard_counts(self: &Arc<Self>, sql: &str, plan: Option<&Plan>, params: &Params, after: &After, timeout_ms: u64) -> Result<(usize, Vec<u64>), ApiError> {
        let t0 = Instant::now();
        let bound = match plan {
            Some(p) => p.clone(),
            None => self.bound_read(sql, params, self.timeout_of(timeout_ms)).await?,
        };
        if bound.root.uses_operators() {
            return Err(ApiError::bad("query error: a guard's levels are counted over one table's rows: a query without subqueries"));
        }
        let schema = self.table(bound.table()).await?;
        let select = bound.to_select(&schema)?;
        let timeout = self.timeout_of(timeout_ms);
        let deadline = t0 + timeout;
        match &self.backend {
            Backend::Local(db) => {
                let snap = self.read_snapshot(db, 0, after, deadline).await?;
                let view = snap.table(&schema.table).ok_or_else(|| ApiError::bad(format!("query error: no such table: {}", schema.table)))?;
                let Some(permit) = self.admit(timeout).await else { return Err(self.rejected("guard_counts", t0)) };
                let db = db.clone();
                let synonyms = self.synonyms().or_else(|| db.exec.synonyms.clone());
                let opts = ExecOptions { deadline: Some(deadline), memory: self.query_memory.clone(), synonyms, ..db.exec.clone() };
                let r = tokio::task::spawn_blocking(move || db.guard_counts(&view, &select, &opts)).await.map_err(ApiError::internal)?;
                drop(permit);
                Ok(r?)
            }
            Backend::Cluster(c) => c.guard_counts(sql, &schema, plan, params, &select, after, deadline).await,
        }
    }

    /// The codes that can follow each of `prefixes` in semantic-ID field
    /// `field`, among the rows `sql` (`SELECT COUNT(*) FROM t WHERE ...`)
    /// or `plan` filters, seeing the writes `after` names: per prefix, each
    /// next code and its rows ([`chloedb::db::Db::sid_next`]), summed over
    /// the shards on a coordinator. It takes a query slot.
    #[allow(clippy::too_many_arguments)]
    pub async fn sid_next(self: &Arc<Self>, sql: &str, plan: Option<&Plan>, params: &Params, field: &str, prefixes: &[String], after: &After, timeout_ms: u64) -> Result<Vec<Vec<(String, u64)>>, ApiError> {
        let t0 = Instant::now();
        let bound = match plan {
            Some(p) => p.clone(),
            None => self.bound_read(sql, params, self.timeout_of(timeout_ms)).await?,
        };
        if bound.root.uses_operators() {
            return Err(ApiError::bad("query error: the next codes count one table's rows: a condition without subqueries"));
        }
        let schema = self.table(bound.table()).await?;
        let select = bound.to_select(&schema)?;
        let timeout = self.timeout_of(timeout_ms);
        let deadline = t0 + timeout;
        match &self.backend {
            Backend::Local(db) => {
                let snap = self.read_snapshot(db, 0, after, deadline).await?;
                let view = snap.table(&schema.table).ok_or_else(|| ApiError::bad(format!("query error: no such table: {}", schema.table)))?;
                let Some(permit) = self.admit(timeout).await else { return Err(self.rejected("sid_next", t0)) };
                let db = db.clone();
                let opts = ExecOptions { deadline: Some(deadline), memory: self.query_memory.clone(), ..db.exec.clone() };
                let (field, prefixes) = (field.to_string(), prefixes.to_vec());
                let r = tokio::task::spawn_blocking(move || db.sid_next_with(&view, &select, &field, &prefixes, &opts)).await.map_err(ApiError::internal)?;
                drop(permit);
                Ok(r?)
            }
            Backend::Cluster(c) => c.sid_next(sql, &schema, plan, params, field, prefixes, after, deadline).await,
        }
    }

    /// Deletes the rows whose `expire_by` time has passed
    /// ([`Db::expire`]), as a server that takes writes does every few
    /// seconds ([`crate::server::spawn_expirer`]): not a replica, which
    /// applies its primary's deletes, nor a server that cannot write, nor
    /// one that holds rows of slots moving here or away, which the move
    /// copies or cleans up. Reads leave expired rows out either way; this
    /// frees their space. Returns how many it deleted.
    pub fn expire_due(&self) -> chloedb::Result<u64> {
        let Backend::Local(db) = &self.backend else { return Ok(0) };
        if matches!(self.replication(), Replication::Replica(_)) || self.failing().is_some() || self.moving.mask().is_some() {
            return Ok(0);
        }
        if !db.catalog().iter().any(|t| t.expire_by.is_some()) {
            return Ok(0);
        }
        db.expire(chloedb::timestamp::now(), EXPIRE_BATCH)
    }

    /// Refreshes the database's rollups ([`Db::refresh_rollup`]), every
    /// bucket of each when `full`, as a server that takes writes does
    /// every few seconds ([`crate::server::spawn_rollups`]): not a replica,
    /// which applies the rollups' writes its primary ships. Each rollup is
    /// refreshed on its own, so that one whose refresh fails leaves the
    /// others to go on ([`Db::refresh_rollups`]): each rollup's name and
    /// what its refresh did, or why it failed.
    pub fn refresh_rollups_due(&self, full: bool) -> chloedb::Result<Vec<(String, chloedb::Result<chloedb::db::RefreshStats>)>> {
        let Backend::Local(db) = &self.backend else { return Ok(Vec::new()) };
        if matches!(self.replication(), Replication::Replica(_)) || self.failing().is_some() || self.moving.mask().is_some() {
            return Ok(Vec::new());
        }
        db.refresh_rollups(if full { chloedb::query::rollup::Refresh::Full } else { chloedb::query::rollup::Refresh::Recent })
    }

    /// Deletes the database's records of idempotency keys older than
    /// [`idem::TTL`] ([`idem::forget_expired`]), as a server that takes
    /// writes does every minute ([`crate::server::spawn_key_forgetter`]):
    /// not a replica, which applies its primary's deletes, nor a server
    /// that cannot write. Retries ignore an expired key's record either
    /// way. Returns how many it deleted.
    pub fn forget_keys_due(&self) -> Result<usize, ApiError> {
        let Backend::Local(db) = &self.backend else { return Ok(0) };
        if matches!(self.replication(), Replication::Replica(_)) || self.failing().is_some() {
            return Ok(0);
        }
        // Held like a client's write, so that a primary handing its shard
        // over waits for it.
        let _by = self.admit_write()?;
        idem::forget_expired(db, chloedb::timestamp::now(), Instant::now() + self.timeout)
    }

    /// Rows of table `table` (the first without one) by primary key,
    /// read without SQL ([`Db::get_many_at`]): per key, its live row as
    /// `SELECT *` answers it and the number of the write that last changed
    /// it, or none. A coordinator asks each key's shard. At most
    /// [`MAX_GET_KEYS`] keys. With `columns`, of those columns alone, in
    /// that order ([`Db::get_columns_at`]); a coordinator reads the
    /// shards' whole rows and keeps those columns.
    /// With `count`, a client's read, its keys count as asked for
    /// ([`crate::reads`]).
    pub async fn get_rows(self: &Arc<Self>, table: Option<&str>, keys: Vec<Value>, columns: Option<Vec<String>>, after: &After, timeout_ms: u64, count: bool) -> Result<KeyRows, ApiError> {
        let t0 = Instant::now();
        if keys.len() > MAX_GET_KEYS {
            return Err(ApiError::bad(format!("a read by key takes at most {MAX_GET_KEYS} keys, not {}", keys.len())));
        }
        let schema = match table {
            Some(t) => self.table(t).await?,
            None => self.default_table()?,
        };
        let pk = schema.primary_key.ok_or_else(|| ApiError::bad(format!("{} has no primary key: rows are read by key only from a table with one", schema.table)))?;
        let keys: Vec<Value> = keys.iter().map(|k| chloedb::db::key_value(&schema, pk, k)).collect::<chloedb::Result<_>>()?;
        if let Some(c) = &columns {
            Db::key_read_fields(&schema, c)?;
        }
        let timeout = self.timeout_of(timeout_ms);
        let deadline = t0 + timeout;
        let r = match &self.backend {
            Backend::Local(db) => {
                let snap = self.read_snapshot(db, 0, after, deadline).await?;
                let view = snap.table(&schema.table).ok_or_else(|| ApiError::bad(format!("query error: no such table: {}", schema.table)))?;
                let Some(permit) = self.admit(timeout).await else { return Err(self.rejected("get", t0)) };
                let db = db.clone();
                let opts = ExecOptions { deadline: Some(deadline), ..db.exec.clone() };
                let asked = if count && self.reads.on() { keys.clone() } else { Vec::new() };
                let r = tokio::task::spawn_blocking(move || {
                    let s = view.schema();
                    let (names, rows) = match &columns {
                        None => (Db::get_columns(s), db.get_many_at(&view, &keys, &opts)),
                        Some(c) => (Db::key_read_fields(s, c)?.into_iter().map(|i| s.fields[i].name.clone()).collect(), db.get_columns_at(&view, &keys, c, &opts)),
                    };
                    rows.map(|rows| KeyRows { table: s.table.clone(), columns: names, rows: rows.into_iter().map(|r| r.map(|r| (r.row, r.seq_no))).collect() })
                })
                .await
                .map_err(ApiError::internal)?;
                drop(permit);
                if let Ok(got) = &r {
                    let found: Vec<bool> = got.rows.iter().map(Option::is_some).collect();
                    self.reads.asked(&schema.table, crate::reads::At::here(), &asked, &found);
                }
                r.map_err(ApiError::from)
            }
            Backend::Cluster(c) => c.get(&schema, &keys, after, deadline).await.map(|(rows, served)| {
                if count && self.reads.on() {
                    for s in served {
                        self.reads.asked(&schema.table, s.at, &s.keys, &s.found);
                    }
                }
                match &columns {
                    None => Ok(KeyRows { table: schema.table.clone(), columns: Db::get_columns(&schema), rows }),
                    Some(c) => {
                        // The shards answer every column `SELECT *` does.
                        let star: Vec<usize> = chloedb::query::sql::star_fields(&schema).collect();
                        let fields = Db::key_read_fields(&schema, c)?;
                        let at: Vec<usize> = fields.iter().map(|f| star.iter().position(|s| s == f).expect("a column of a read by key")).collect();
                        let rows = rows.into_iter().map(|r| r.map(|(row, n)| (at.iter().map(|&j| row.get(j).cloned().unwrap_or(Value::Null)).collect(), n))).collect();
                        Ok(KeyRows { table: schema.table.clone(), columns: fields.iter().map(|&i| schema.fields[i].name.clone()).collect(), rows })
                    }
                }
            })
            .and_then(|r: Result<KeyRows, ApiError>| r),
        };
        telemetry::record("get", t0, if r.is_ok() { "ok" } else { "error" });
        r
    }

    /// The paths listing `call` asks for ([`chloedb::query::paths`]): of
    /// this database's rows, or of every shard's, summed.
    pub async fn json_paths(self: &Arc<Self>, call: &Call, params: &Params, after: &After, timeout_ms: u64) -> Result<Paths, ApiError> {
        let t0 = Instant::now();
        let timeout = self.timeout_of(timeout_ms);
        let deadline = t0 + timeout;
        call.field_of(&*self.table(&call.table).await?)?;
        match &self.backend {
            Backend::Local(db) => {
                let snap = self.read_snapshot(db, 0, after, deadline).await?;
                let Some(permit) = self.admit(timeout).await else { return Err(self.rejected("paths", t0)) };
                let db = db.clone();
                let opts = ExecOptions { deadline: Some(deadline), memory: self.query_memory.clone(), ..db.exec.clone() };
                let (call, params) = (call.clone(), params.clone());
                let r = tokio::task::spawn_blocking(move || db.json_paths_at(&snap, &call, &params, &opts)).await.map_err(ApiError::internal)?;
                drop(permit);
                Ok(r?)
            }
            Backend::Cluster(c) => c.json_paths(call, params, after, deadline).await,
        }
    }

    /// Any statement, on any table: rows for a SELECT, which sees the
    /// writes `after` names; the documents an INSERT, UPDATE or DELETE
    /// wrote; none for `CREATE TABLE`, `DROP TABLE` and the `RANK PROFILE`
    /// statements ([`Self::ddl`]).
    /// The statement needs the scope of its kind ([`command_scope`]), which
    /// `who` must have; a table's or rank profile's statement is audited.
    pub async fn sql(self: &Arc<Self>, who: &Principal, sql: &str, params: &Params, after: After, timeout_ms: u64) -> Result<SqlAnswer, ApiError> {
        self.sql_in(who, sql, params, after, timeout_ms, None, None).await
    }

    /// [`Self::sql`]; with `slots`, an UPDATE or DELETE over the rows of
    /// those slots alone, as a coordinator sends one again to the shard
    /// slots moved to while it ran. With an idempotency `key`, a write
    /// statement sent again after it committed answers its first answer
    /// rather than applying again; through a coordinator, each shard's part
    /// is kept under the key ([`Coordinator::sql`]).
    #[allow(clippy::too_many_arguments)]
    pub async fn sql_in(self: &Arc<Self>, who: &Principal, sql: &str, params: &Params, after: After, timeout_ms: u64, slots: Option<Vec<usize>>, key: Option<String>) -> Result<SqlAnswer, ApiError> {
        self.statement_fits(sql)?;
        if let Some(st) = chloedb::query::classify::parse_statement(sql)? {
            return self.classifier_statement(who, &st, timeout_ms).await;
        }
        // `CREATE RECOMMENDER`, `DROP RECOMMENDER`: on this database, whose
        // writes then feed the counts ([`chloedb::query::recommend`]).
        if chloedb::query::recommend::parse_statement(sql)?.is_some() {
            who.may(Scope::Admin, "a recommender statement")?;
            let Backend::Local(db) = &self.backend else {
                return Err(ApiError::bad("CREATE RECOMMENDER does not run through a coordinator: a recommender counts the events of one database, and a user's events are on several shards (see cluster-limits)"));
            };
            keys::note(obs::truncate(sql, 500).into_owned());
            let t0 = Instant::now();
            let (db, text) = (db.clone(), sql.to_string());
            let r = tokio::task::spawn_blocking(move || db.sql(&text, &Params::new())).await.map_err(ApiError::internal)??;
            self.learn_tables(true).await;
            let took_ms = t0.elapsed().as_secs_f64() * 1e3;
            return Ok(match r {
                chloedb::db::SqlResult::Rows(rs) => SqlAnswer::Rows(Answer { rs, took_ms, kind: "recommender", missing: Vec::new(), lease: 0, arrow: Vec::new(), stale: false, cursor: None }),
                chloedb::db::SqlResult::Written(_) => SqlAnswer::Written { w: Written::default(), took_ms },
            });
        }
        // `CREATE ROLLUP`, `REFRESH ROLLUP`, `DROP ROLLUP`
        // ([`chloedb::query::rollup`]).
        if let Some(st) = chloedb::query::rollup::parse_statement(sql)? {
            return self.rollup_statement(who, st, sql, timeout_ms).await;
        }
        // `CREATE SYNONYMS ON t AS '...'`: the table's synonym list, kept
        // as a function ([`chloedb::query::table_synonyms`]).
        if let Some(st) = chloedb::query::table_synonyms::parse_statement(sql)? {
            if !self.catalog().iter().any(|s| s.table.eq_ignore_ascii_case(st.table())) {
                return Err(ApiError::bad(format!("query error: no table {}", st.table())));
            }
            return Box::pin(self.sql_in(who, &st.function_sql()?, &Params::new(), after, timeout_ms, None, None)).await;
        }
        // `SELECT ... FROM chunks_of(...)`, a query; `INSERT INTO t (...)
        // SELECT ... FROM chunks_of(...)`, the chunks found, then their
        // INSERTs.
        if chloedb::query::chunks::of_sql(sql, params)?.is_some() {
            who.may(Scope::Read, "a query")?;
            let a = self.query(QueryReq { sql: sql.into(), params: params.clone(), timeout_ms, after, who: Some(who.clone()), ..Default::default() }).await?;
            return Ok(SqlAnswer::Rows(a));
        }
        if let Some(ins) = chloedb::query::chunks::insert_of_sql(sql, params)? {
            who.may(Scope::Write, "a write statement")?;
            let t0 = Instant::now();
            let schema = self.catalog().into_iter().find(|s| s.table.eq_ignore_ascii_case(&ins.query.table)).ok_or_else(|| ApiError::bad(format!("query error: no table {}", ins.query.table)))?;
            let mut rows = ins.query.source_rows();
            let chunks = loop {
                let a = self.query(QueryReq { sql: ins.query.source_sql_of(&schema, rows)?, params: params.clone(), timeout_ms, after: after.clone(), own_read: true, who: Some(who.clone()), ..Default::default() }).await?;
                let chunks = ins.query.finish(&a.rs);
                match ins.query.more_rows(rows, &a.rs, &chunks) {
                    Some(more) => rows = more,
                    None => break chunks,
                }
            };
            let mut total = Written::default();
            for (statement, p) in ins.inserts(&chunks) {
                if let SqlAnswer::Written { w, .. } = Box::pin(self.sql_in(who, &statement, &p, After::Any, timeout_ms, None, None)).await? {
                    total.rows += w.rows;
                    total.skipped += w.skipped;
                    total.seq_no = w.seq_no.or(total.seq_no);
                    total.seq_nos.extend(w.seq_nos);
                }
            }
            return Ok(SqlAnswer::Written { w: total, took_ms: t0.elapsed().as_secs_f64() * 1e3 });
        }
        let command = self.bound(|tables| bind_command(sql, tables, params)).await?;
        if let Command::Function(f) = &command {
            // How a coordinator numbers a function's versions alike on
            // every shard.
            if f.at().is_some() && !who.root {
                return Err(ApiError::bad("query error: AT VERSION is for a coordinator's own statements: CREATE OR REPLACE FUNCTION makes the next version"));
            }
        }
        let need = command_scope(&command);
        // An UPDATE or DELETE with a LIMIT, which a coordinator runs by
        // picking its rows first ([`Self::limited_write`]).
        let limited = matches!(&command, Command::Write { write: chloedb::query::Write::Update { select, .. } | chloedb::query::Write::Delete { select }, .. } if select.limit.is_some());
        who.may(need, &format!("a {} statement", match need { Scope::Feed | Scope::Read => "query", Scope::Write => "write", Scope::Admin => "table, rank profile or function" }))?;
        if who.limited() {
            crate::scoped::check_statement(who, sql, &command, &self.table_names(), &self.functions())?;
        }
        // The connectors' configs, runs and state are the server's own.
        for t in changed_tables(&command) {
            crate::connectors::may_change(t, "a statement")?;
        }
        if need == Scope::Admin {
            keys::note(obs::truncate(sql, 500).into_owned());
        }
        let ddl = match command {
            Command::Query => {
                let a = self.query(QueryReq { sql: sql.into(), params: params.clone(), timeout_ms, after, who: Some(who.clone()), ..Default::default() }).await?;
                return Ok(SqlAnswer::Rows(a));
            }
            Command::Explain { .. } => return Err(ApiError::bad("query error: EXPLAIN runs through POST /v1/explain")),
            // ONB-16: the tables' names, as GET /v1/tables lists them for
            // this key (a coordinator's from what its shards hold).
            Command::ShowTables => {
                let t0 = Instant::now();
                let names: Vec<String> = self.tables_listed().await.iter().filter(|s| who.table_scope(&s.table).is_some_and(|x| x >= Scope::Read)).map(|s| s.table.clone()).collect();
                let rs = chloedb::query::sql::table_list(&names);
                return Ok(SqlAnswer::Rows(Answer { rs, took_ms: t0.elapsed().as_secs_f64() * 1e3, kind: "tables", missing: Vec::new(), lease: 0, arrow: Vec::new(), stale: false, cursor: None }));
            }
            Command::CreateTable { .. } | Command::DropTable { .. } | Command::CreateRankProfile { .. } | Command::DropRankProfile { .. } | Command::Function(_) => true,
            Command::AlterTable { alters, .. } => {
                // A coordinator's plans name a table's columns by their
                // place, and its shards drop a column one by one: a query
                // planned before a shard dropped one would read the next.
                if matches!(self.backend, Backend::Cluster(_)) && alters.iter().any(|a| matches!(a, Alter::DropField(_))) {
                    return Err(ApiError::bad("ALTER TABLE ... DROP COLUMN does not run through a coordinator yet (its plans name columns by place); ADD COLUMN and RENAME COLUMN do"));
                }
                true
            }
            Command::Write { .. } => false,
        };
        if after != After::Any {
            return Err(ApiError::bad(FOR_READS));
        }
        let t0 = Instant::now();
        let r = match (&self.backend, ddl) {
            (_, true) => self.ddl(sql, params, timeout_ms, &who.name).await,
            (Backend::Local(db), false) => {
                let only = slots.as_ref().map(|s| moving::mask_outside(s));
                let key = key.map(|k| (k, statement_hash(sql, params, slots.as_deref())));
                let mut tries = 0;
                loop {
                    let (text, p, only) = (sql.to_string(), params.clone(), only.clone());
                    let refused = Arc::new(Mutex::new(None));
                    let guard = self.statement_guard(routed(), refused.clone());
                    let prepare = move |db: &Db| {
                        let Some(w) = db.prepare_write(&text, &p)? else { return Ok(None) };
                        let w = match only {
                            Some(m) => w.masked(m)?,
                            None => w,
                        };
                        Ok(Some(w.guarded(guard)))
                    };
                    let r = self.write_local(db, prepare, timeout_ms, key.clone()).await;
                    let refusal = refused.lock().unwrap_or_else(|e| e.into_inner()).take();
                    match (refusal, r) {
                        (Some(e), Err(_)) => break Err(e),
                        // An ALTER TABLE committed between binding the
                        // statement and writing it, which wrote nothing:
                        // it is bound again, to the table as it is now.
                        (None, Err(e)) if tries < chloedb::db::ALTERED_TRIES && chloedb::db::altered_while_running(&e.message) => tries += 1,
                        (_, r) => break r,
                    }
                }
            }
            (Backend::Cluster(_), false) if limited => self.limited_write(sql, params, key).await,
            (Backend::Cluster(c), false) => c.sql(sql, params, key).await,
        };
        let w = self.wrote(r, t0, Some(sql))?;
        Ok(SqlAnswer::Written { w, took_ms: t0.elapsed().as_secs_f64() * 1e3 })
    }

    /// A rollup's statement ([`chloedb::query::rollup`]), on a database of
    /// its own: `CREATE ROLLUP` and `DROP ROLLUP` as table statements are
    /// ([`Self::ddl`]), `REFRESH ROLLUP` answered with what it did.
    async fn rollup_statement(self: &Arc<Self>, who: &Principal, st: chloedb::query::rollup::Statement, sql: &str, timeout_ms: u64) -> Result<SqlAnswer, ApiError> {
        use chloedb::query::rollup::Statement;
        who.may(Scope::Admin, "a rollup statement")?;
        if who.limited() {
            return Err(ApiError::bad("query error: a rollup statement needs a key that is not limited to tables"));
        }
        let Backend::Local(db) = &self.backend else {
            return Err(ApiError::bad("query error: rollups do not run through a coordinator yet; each shard keeps its own"));
        };
        let t0 = Instant::now();
        match st {
            Statement::Refresh { name, mode } => {
                self.takes_writes()?;
                let db = db.clone();
                let s = tokio::task::spawn_blocking(move || db.refresh_rollup_with(&name, mode)).await.map_err(ApiError::internal)??;
                let columns = ["buckets_checked", "buckets_folded", "rows_written", "seconds"].map(String::from).to_vec();
                let rows = vec![vec![Value::Int(s.checked as i64), Value::Int(s.folded as i64), Value::Int(s.rows_written as i64), Value::Float(s.seconds)]];
                let rs = ResultSet { columns, rows, keys: Vec::new(), scores: Vec::new(), parts: None, stats: Default::default() };
                Ok(SqlAnswer::Rows(Answer { rs, took_ms: t0.elapsed().as_secs_f64() * 1e3, kind: "rollup", missing: Vec::new(), lease: 0, arrow: Vec::new(), stale: false, cursor: None }))
            }
            Statement::Create { rollup, if_not_exists } => {
                // Made here. A table of up to FILL_NOW rows is folded at
                // once, so the rollup answers as soon as the statement
                // does; a longer one's first fold, which takes minutes, is
                // the refresher's, a few seconds on.
                keys::note(obs::truncate(sql, 500).into_owned());
                self.takes_writes()?;
                let (name, base, shown) = (rollup.name.clone(), rollup.base.clone(), rollup.name.clone());
                let made = {
                    let _entry = self.moving.enter();
                    self.not_held()?;
                    let by = self.admitted();
                    let timeout = self.timeout_of(timeout_ms);
                    let db = db.clone();
                    let made = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let m = made.clone();
                    let w = tokio::task::spawn_blocking(move || {
                        wait_for_writer(Instant::now() + timeout, || {
                            m.store(db.create_rollup(&rollup, if_not_exists, &db.exec)?, std::sync::atomic::Ordering::Relaxed);
                            Ok(Written { seq_no: db.storage().seq_no().map(|n| n.seq), ..Default::default() })
                        })
                    })
                    .await
                    .map_err(ApiError::internal)??;
                    self.replicated(w, timeout, by).await?;
                    made.load(std::sync::atomic::Ordering::Relaxed)
                };
                self.learn_tables(true).await;
                const FILL_NOW: i64 = 1_000_000;
                let db = db.clone();
                let filled = tokio::task::spawn_blocking(move || -> chloedb::Result<Option<chloedb::db::RefreshStats>> {
                    if !made {
                        return Ok(None);
                    }
                    let rows = db.query(&format!("SELECT COUNT(*) FROM {}", chloedb::query::sql::ident(&base)), &Params::new())?;
                    match rows.rows.first().and_then(|r| r.first()) {
                        Some(Value::Int(n)) if *n <= FILL_NOW => db.refresh_rollup(&name, false).map(Some),
                        _ => Ok(None),
                    }
                })
                .await
                .map_err(ApiError::internal)?
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "could not fill a new rollup at once; the refresher will");
                    None
                });
                let note = match (made, &filled) {
                    (false, _) => "the rollup was there already: nothing changed".to_string(),
                    (true, Some(_)) => "made and filled: query it by its name now; it is kept up to date a few seconds after rows arrive".to_string(),
                    (true, None) => "made; the refresher fills it in the background (minutes for a long table): until then it has fewer rows, and REFRESH ROLLUP fills it now".to_string(),
                };
                let done = filled.is_some();
                let s = filled.unwrap_or_default();
                let columns = ["rollup", "made", "filled", "buckets_folded", "rows_written", "note"].map(String::from).to_vec();
                let rows = vec![vec![
                    Value::Str(shown),
                    Value::Bool(made),
                    Value::Bool(done),
                    Value::Int(s.folded as i64),
                    Value::Int(s.rows_written as i64),
                    Value::Str(note),
                ]];
                let rs = ResultSet { columns, rows, keys: Vec::new(), scores: Vec::new(), parts: None, stats: Default::default() };
                Ok(SqlAnswer::Rows(Answer { rs, took_ms: t0.elapsed().as_secs_f64() * 1e3, kind: "rollup", missing: Vec::new(), lease: 0, arrow: Vec::new(), stale: false, cursor: None }))
            }
            Statement::Drop { .. } => {
                keys::note(obs::truncate(sql, 500).into_owned());
                let w = self.ddl(sql, &Params::new(), timeout_ms, &who.name).await?;
                Ok(SqlAnswer::Written { w, took_ms: t0.elapsed().as_secs_f64() * 1e3 })
            }
        }
    }

    /// `CREATE CLASSIFIER`: the rows it trains on read as a query reads
    /// them (a coordinator's from every shard), the classifier trained
    /// here and kept as a stored function ([`Self::ddl`]), answered with
    /// what the training measured; `DROP CLASSIFIER`, the function's drop
    /// ([`chloedb::query::classify`]).
    async fn classifier_statement(self: &Arc<Self>, who: &Principal, st: &chloedb::query::classify::Statement, timeout_ms: u64) -> Result<SqlAnswer, ApiError> {
        use chloedb::query::classify::Statement;
        who.may(Scope::Admin, "a classifier statement")?;
        let t0 = Instant::now();
        let c = match st {
            Statement::Drop { .. } => {
                let sql = st.drop_sql().expect("a drop");
                keys::note(sql.clone());
                let w = self.ddl(&sql, &Params::new(), timeout_ms, &who.name).await?;
                return Ok(SqlAnswer::Written { w, took_ms: t0.elapsed().as_secs_f64() * 1e3 });
            }
            Statement::Create(c) => c,
        };
        let schema = self.table(&c.table).await?;
        c.check(&schema)?;
        if !c.replace && self.functions().current(&c.name).is_some() {
            return Err(ApiError::bad(format!("query error: function {} exists: CREATE OR REPLACE CLASSIFIER {} makes its next version", c.name, c.name)));
        }
        let read = self.query(QueryReq { sql: c.training_sql(), timeout_ms, own_read: true, ..Default::default() }).await?;
        let (c2, schema2) = (c.clone(), schema.clone());
        let trained = tokio::task::spawn_blocking(move || c2.train(&schema2, &read.rs)).await.map_err(ApiError::internal)??;
        let sql = trained.function_sql(c);
        keys::note(format!("CREATE CLASSIFIER {} ON {}({}) LABELS {}", c.name, c.table, c.field, c.labels));
        self.ddl(&sql, &Params::new(), timeout_ms, &who.name).await?;
        Ok(SqlAnswer::Rows(Answer { rs: trained.result_set(), took_ms: t0.elapsed().as_secs_f64() * 1e3, kind: "classifier", missing: Vec::new(), lease: 0, arrow: Vec::new(), stale: false, cursor: None }))
    }

    /// The classes of `text` by the classifier `name`, its `k` best: the
    /// text embedded by the model of the field the classifier was trained
    /// on (through a coordinator, as a query's `{"embed": ...}` parameter
    /// is).
    pub async fn classify(self: &Arc<Self>, name: &str, text: &str, k: usize) -> Result<chloedb::query::classify::Classified, ApiError> {
        // A database of its own embeds with the models it was given.
        if let Backend::Local(db) = &self.backend {
            let (db, name, text) = (db.clone(), name.to_string(), text.to_string());
            return Ok(tokio::task::spawn_blocking(move || db.classify(&name, &text, k)).await.map_err(ApiError::internal)??);
        }
        let functions = self.functions();
        let f = functions.current(name).ok_or_else(|| ApiError::bad(format!("no classifier named {name}")))?;
        let c = chloedb::query::classify::of_function(f)?.ok_or_else(|| ApiError::bad(format!("function {name} is not a classifier CREATE CLASSIFIER made")))?;
        let p = self.params(None, Some(&json!({ "v": { "embed": text, "field": c.field, "table": c.table } }))).await?;
        let Some(Value::Vector(x)) = p.0.get("v") else { return Err(ApiError::bad(format!("{}.{} gave no vector of the text", c.table, c.field))) };
        Ok(chloedb::query::classify::Classified::of(name, f.version, &c, x, k)?)
    }

    /// Synonyms mined from a table ([`chloedb::query::mine`]): on a
    /// database of its own, which holds the rows and the models.
    pub async fn mine_synonyms(self: &Arc<Self>, req: chloedb::query::mine::Request) -> Result<chloedb::query::mine::Mined, ApiError> {
        let Backend::Local(db) = &self.backend else {
            return Err(ApiError::bad("synonyms are mined on a server of its own (a shard holds the rows and models of a part); run CREATE SYNONYMS here with what it proposes"));
        };
        let db = db.clone();
        Ok(tokio::task::spawn_blocking(move || db.mine_synonyms(&req)).await.map_err(ApiError::internal)??)
    }

    /// The synonym lists of the tables that have one
    /// ([`chloedb::query::table_synonyms`]): table, rules, version.
    pub fn table_synonyms(&self) -> Vec<(String, String, i64)> {
        let functions = self.functions();
        let mut out: Vec<(String, String, i64)> = self
            .catalog()
            .iter()
            .filter_map(|s| {
                let f = functions.current(&chloedb::query::table_synonyms::function_name(&s.table))?;
                Some((s.table.clone(), chloedb::query::table_synonyms::rules_of(f)?, f.version))
            })
            .collect();
        out.sort();
        out
    }

    /// `UPDATE ... LIMIT n` or `DELETE ... [ORDER BY ...] LIMIT n` through a
    /// coordinator, whose shards would each take n rows: the keys of the
    /// rows it changes are read first, by a SELECT of the statement's
    /// WHERE, ORDER BY and LIMIT over every shard, and the statement then
    /// runs without its LIMIT over those keys alone (`WHERE (...) AND key
    /// IN (...)`), so that it changes at most n rows. It is two steps, not
    /// one: a row changed in between so that it no longer matches is left
    /// alone (the WHERE still holds it), and fewer than n rows may change.
    async fn limited_write(self: &Arc<Self>, sql: &str, params: &Params, key: Option<String>) -> Result<Written, ApiError> {
        use sqlparser::ast as a;
        let Backend::Cluster(c) = &self.backend else { return Err(ApiError::internal("limited_write is a coordinator's")) };
        let mut stmts = chloedb::query::parse::parse(sql)?;
        let [stmt] = &mut stmts[..] else { return Err(ApiError::bad("one statement at a time")) };
        let (relation, selection, order_by, limit) = match stmt {
            a::Statement::Update { table, selection, limit, .. } => (table.relation.clone(), selection, Vec::new(), limit),
            a::Statement::Delete(d) => {
                let (a::FromTable::WithFromKeyword(from) | a::FromTable::WithoutKeyword(from)) = &d.from;
                let relation = from.first().map(|t| t.relation.clone()).ok_or_else(|| ApiError::bad("DELETE from one table"))?;
                (relation, &mut d.selection, std::mem::take(&mut d.order_by), &mut d.limit)
            }
            _ => return Err(ApiError::bad("not an UPDATE or DELETE")),
        };
        let a::TableFactor::Table { name, .. } = &relation else { return Err(ApiError::bad("UPDATE or DELETE one table")) };
        let schema = self.table(&name.to_string()).await?;
        let pk = schema.primary_key.ok_or_else(|| ApiError::bad(format!("{} has no primary key: UPDATE and DELETE ... LIMIT through a coordinator pick their rows by key", schema.table)))?;
        let pk_name = schema.fields[pk].name.clone();
        let n = limit.as_ref().map(|l| l.to_string()).unwrap_or_default();
        let cap: usize = n.trim().parse().map_err(|_| ApiError::bad(format!("LIMIT {n}: through a coordinator, an UPDATE or DELETE takes a LIMIT that is a number")))?;
        if cap > MAX_GET_KEYS {
            return Err(ApiError::bad(format!("LIMIT {cap}: through a coordinator, an UPDATE or DELETE takes a LIMIT of at most {MAX_GET_KEYS}")));
        }
        let mut pick = format!("SELECT {} FROM {relation}", ident(&pk_name));
        if let Some(w) = selection.as_ref() {
            pick.push_str(&format!(" WHERE {w}"));
        }
        if !order_by.is_empty() {
            pick.push_str(&format!(" ORDER BY {}", order_by.iter().map(|o| o.to_string()).collect::<Vec<_>>().join(", ")));
        }
        pick.push_str(&format!(" LIMIT {cap}"));
        let picked = self.query(QueryReq { sql: pick, params: params.clone(), ..Default::default() }).await?;
        let keys: Vec<Value> = picked.rs.rows.iter().filter_map(|r| r.first().cloned()).collect();
        if keys.is_empty() {
            return Ok(Written { receipt: None, ..Default::default() });
        }
        let mut params = params.clone();
        let mut list = Vec::with_capacity(keys.len());
        for (j, k) in keys.into_iter().enumerate() {
            let name = format!("__limit_key_{j}");
            list.push(a::Expr::value(a::Value::Placeholder(format!(":{name}"))));
            params.0.insert(name, k);
        }
        let within = a::Expr::InList { expr: Box::new(a::Expr::Identifier(a::Ident::new(pk_name))), list, negated: false };
        *selection = Some(match selection.take() {
            Some(w) => a::Expr::BinaryOp { left: Box::new(a::Expr::Nested(Box::new(w))), op: a::BinaryOperator::And, right: Box::new(within) },
            None => within,
        });
        *limit = None;
        c.sql(&stmt.to_string(), &params, key).await
    }

    /// `CREATE TABLE`, `ALTER TABLE` or `DROP TABLE`, `CREATE RANK PROFILE`
    /// or `DROP RANK PROFILE`: on a local database, once no write holds the writer;
    /// through a coordinator, on every shard ([`Coordinator::ddl`]). A
    /// replica refuses it, as it refuses every write. Each is a numbered
    /// transaction in a database that numbers its writes, so a primary
    /// ships it as it ships a write, and answers once the copies its acks
    /// name hold it.
    ///
    /// A stored function's version records `author`, the name of the key
    /// that wrote it.
    async fn ddl(self: &Arc<Self>, sql: &str, params: &Params, timeout_ms: u64, author: &str) -> Result<Written, ApiError> {
        let r = match &self.backend {
            Backend::Local(db) => {
                self.takes_writes()?;
                // Counted in until it commits, as a write is ([`Self::write`]).
                let _entry = self.moving.enter();
                self.not_held()?;
                let by = self.admitted();
                let timeout = self.timeout_of(timeout_ms);
                let (db, sql, params, who) = (db.clone(), sql.to_string(), params.clone(), author.to_string());
                let w = tokio::task::spawn_blocking(move || {
                    wait_for_writer(Instant::now() + timeout, || {
                        match chloedb::query::function::parse_statement(&sql)? {
                            Some(f) => drop(db.define_function(&f, Some(&who))?),
                            None => drop(db.sql(&sql, &params)?),
                        }
                        // Its number, or a later one.
                        Ok(Written { seq_no: db.storage().seq_no().map(|n| n.seq), ..Default::default() })
                    })
                })
                .await
                .map_err(ApiError::internal)?;
                match w {
                    Ok(w) => self.replicated(w, timeout, by).await,
                    e => e,
                }
            }
            Backend::Cluster(c) => match chloedb::query::function::parse_statement(sql)? {
                Some(f) => c.define_function(&f, author).await,
                None => c.ddl(sql, params).await,
            },
        };
        // The models of the tables it made.
        self.refresh_models();
        r
    }

    /// Applies write operations from a client (`/v1/write`, gRPC `Write`):
    /// [`Self::write_ops`], under a key of its own through a coordinator
    /// when it has none and may span shards ([`Coordinator::keyed`]).
    pub async fn write_request(self: &Arc<Self>, ops: Vec<Json>, key: Option<String>) -> Result<Written, ApiError> {
        let key = match &self.backend {
            Backend::Cluster(c) => c.keyed(&ops, key),
            Backend::Local(_) => key,
        };
        self.write_ops(ops, key).await
    }

    /// Checks write operations `ops` as they would apply now, in a
    /// transaction it rolls back (`WriteRequest.validate`, protocol 19): a
    /// duplicate insert, an `if_seq_no` that does not match, a type or
    /// schema error, a NOT NULL column left empty. A refusal names the
    /// operation, by its place in `ops` ("operation N of this shard's
    /// part: ..."). A writer still busy after [`VALIDATE_WAIT`] leaves the
    /// part unchecked: it passes. A request whose `key` this database recorded passes,
    /// as it will be answered as before. Nothing is held between this and
    /// the write: another client's write in between can still make the
    /// write fail (finding DS-2, the race docs/chloedb/server.md states).
    pub async fn validate_ops(&self, ops: Vec<Json>, key: Option<String>, epoch: u64) -> Result<(), ApiError> {
        let db = self.local().cloned().ok_or_else(|| ApiError::bad("a coordinator checks a write on its shards"))?;
        tokio::task::spawn_blocking(move || {
            if let Some(k) = &key {
                if idem::recorded(&db, k, idem::hash_ops(&ops))?.is_some() {
                    return Ok(());
                }
            }
            // A check is not a write: it waits briefly for a busy writer, and
            // then passes unchecked, so that the part goes into the write
            // queue, whose limits (429) and retries apply as to any write.
            // Waiting as a write does (5 s) held the part out of the queue;
            // answering 503 would send a coordinator looking for another
            // primary.
            match validate_into(&db, &ops, epoch) {
                Err(e) if e.status == 503 => Ok(()),
                r => r,
            }
        })
        .await
        .map_err(ApiError::internal)?
    }

    /// Looks idempotency key `key` up for a request whose operations hash
    /// to `hash`, claiming it if it is new ([`idem::Keys::begin`]): in
    /// memory, then, for a key memory does not hold, in the database
    /// ([`idem::recorded`]), which holds the keys of writes another server
    /// applied before this one was promoted, or this one before it
    /// restarted.
    fn begin_key(&self, db: &Db, key: &str, hash: u64) -> Result<idem::Begin, ApiError> {
        let b = self.idempotency.begin(key, hash);
        if b != idem::Begin::New {
            return Ok(b);
        }
        match idem::recorded(db, key, hash) {
            Ok(None) => Ok(idem::Begin::New),
            // Kept in memory: answered from there from now on.
            Ok(Some(Ok(w))) => {
                self.idempotency.finish(key, Some(w.clone()));
                Ok(idem::Begin::Done(w))
            }
            Ok(Some(Err(()))) => {
                self.idempotency.finish(key, None);
                Ok(idem::Begin::Mismatch)
            }
            Err(e) => {
                self.idempotency.finish(key, None);
                Err(e)
            }
        }
    }

    /// A write `prepare` binds (`None`: not a write), queued for the writer
    /// thread. It is bound, and its vectors computed, before it queues:
    /// models run on a core of their own, not in the writer thread. With
    /// an idempotency key (and the hash of what it names), a retry of a
    /// write that committed answers its first answer, as
    /// [`Self::write_ops`] does.
    async fn write_local<F>(self: &Arc<Self>, db: &Arc<Db>, prepare: F, timeout_ms: u64, key: Option<(String, u64)>) -> Result<Written, ApiError>
    where
        F: FnOnce(&Db) -> chloedb::Result<Option<PreparedWrite>> + Send + 'static,
    {
        let by = self.writable()?;
        self.backpressure(db).await?;
        let q = self.enqueue()?;
        if let Some((k, hash)) = &key {
            match self.begin_key(db, k, *hash)? {
                idem::Begin::New => {}
                idem::Begin::Done(n) => {
                    self.metrics.replay();
                    return self.replicated(n, self.timeout_of(timeout_ms), self.admitted()).await;
                }
                idem::Begin::Busy => return Err(ApiError { code: None, status: 409, message: "a write with this idempotency key is still running".into() }),
                idem::Begin::Mismatch => return Err(ApiError { code: None, status: 422, message: "this idempotency key was used for a different statement".into() }),
            }
        }
        let r = async {
            let core = match self.embeds() {
                true => Some(self.embedding.clone().acquire_owned().await.map_err(ApiError::internal)?),
                false => None,
            };
            let db = db.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                let _core = core;
                prepare(&db)
            })
            .await
            .map_err(ApiError::internal)??
            .ok_or_else(|| ApiError::bad("not a write statement"))?;
            // The deadline bounds the wait for the writer, not the embedding.
            let deadline = Instant::now() + self.timeout_of(timeout_ms);
            self.write(WriteJob::Sql(Box::new(prepared)), deadline, key.clone(), q, Moved::No).await
        }
        .await;
        if let (Err(_), Some((k, _))) = (&r, &key) {
            // Not committed (the writer reports a commit as Ok).
            self.idempotency.finish(k, None);
        }
        self.replicated(r?, self.timeout_of(timeout_ms), by).await
    }

    /// Applies write operations (see the proto's WriteRequest) in one
    /// transaction per server, which other requests waiting at the same
    /// time may share (see [`writer`]). With an idempotency `key`, a retry
    /// of a request that committed returns its answer unapplied.
    pub async fn write_ops(self: &Arc<Self>, ops: Vec<Json>, key: Option<String>) -> Result<Written, ApiError> {
        self.write_ops_as(ops, key, Moved::No).await
    }

    /// [`Self::write_ops`], of rows of moving slots copied by the
    /// coordinator moving them, or deleted here, as `moved` says
    /// ([`Self::write`]).
    pub async fn write_ops_as(self: &Arc<Self>, ops: Vec<Json>, key: Option<String>, moved: Moved) -> Result<Written, ApiError> {
        let received = ops.iter().filter(|op| !is_mark(op)).count() as u64;
        self.write_ops_counted(ops, key, moved).await.map(|mut w| {
            w.received = received;
            w
        })
    }

    async fn write_ops_counted(self: &Arc<Self>, mut ops: Vec<Json>, key: Option<String>, moved: Moved) -> Result<Written, ApiError> {
        let t0 = Instant::now();
        own_marks_ok(&ops)?;
        // The connectors' configs, runs and state are the server's own.
        crate::connectors::may_write_ops(&ops, || self.default_table().ok().map(|t| t.table.clone()))?;
        let n = match &self.backend {
            Backend::Local(db) => {
                // Before any work: a replica, a database behind on its
                // flushes, or a full queue refuses the request; and a
                // client's on a follower (a move's copies and deletes are
                // the cluster's own).
                let own = ops.iter().any(feed::marks_own);
                let carried = following().filter(|_| own).and_then(|n| feed::carried_follows(&ops, &n));
                let copy = following().and_then(|n| feed::carried_copy(&ops, &n));
                // A promotion's own writes end following ([`Self::promote_follower`]).
                let promoting = own && crate::service::own() && ops.iter().any(feed::marks_promoted);
                let writer = match moved {
                    Moved::No if promoting => self.is_writer(),
                    Moved::No => self.is_writer().and_then(|()| self.not_following(carried, copy)),
                    _ => self.is_writer(),
                };
                let admitted = match writer {
                    Ok(()) => self.backpressure(db).await.and_then(|_| self.enqueue()),
                    Err(e) => Err(e),
                };
                let queued = match admitted {
                    Ok(q) => q,
                    Err(e) => return self.wrote(Err(e), t0, None),
                };
                // Hashed as sent, before embedding adds to them.
                let key = key.map(|k| (k, idem::hash_ops(&ops)));
                if let Some((k, hash)) = &key {
                    match self.begin_key(db, k, *hash)? {
                        idem::Begin::New => {}
                        idem::Begin::Done(n) => {
                            self.metrics.replay();
                            // A retry after a write its copies did not all
                            // acknowledge waits for them again.
                            return self.replicated(n, self.timeout, self.admitted()).await;
                        }
                        idem::Begin::Busy => return Err(ApiError { code: None, status: 409, message: "a write with this idempotency key is still running".into() }),
                        idem::Begin::Mismatch => return Err(ApiError { code: None, status: 422, message: "this idempotency key was used for different operations".into() }),
                    }
                }
                // A hold of the shard's writes: from here on it takes no
                // other, and it commits once those under way did.
                let _holding = feed::carried_hold(&ops).filter(|h| h % 2 == 1).map(|_| self.hold_pending());
                if _holding.is_some() && !self.moving.drained(tokio::time::Instant::now() + self.timeout).await {
                    if let Some((k, _)) = &key {
                        self.idempotency.finish(k, None);
                    }
                    return self.wrote(Err(ApiError { code: None, status: 503, message: format!("the writes under way did not commit within {} s, so the writes are not held: try again", self.timeout.as_secs()) }), t0, None);
                }
                let r = async {
                    // Not a retry of a write applied: a primary may refuse
                    // it before applying it.
                    let by = self.admit_write()?;
                    if self.embeds() || self.has_media() {
                        // Queued writes wait for a core to embed on.
                        let core = self.embedding.clone().acquire_owned().await.map_err(ApiError::internal)?;
                        let (svc, db) = (self.clone(), db.clone());
                        ops = tokio::task::spawn_blocking(move || {
                            let _core = core;
                            svc.embed_ops(&db, &mut ops).map(|_| ops)
                        })
                        .await
                        .map_err(ApiError::internal)??;
                    }
                    // The deadline bounds the wait for the writer, not the embedding.
                    let w = self.write(WriteJob::Ops(ops), Instant::now() + self.timeout, key.clone(), queued, moved).await?;
                    // Visible now: how long after its event each row is.
                    let now = chloedb::timestamp::now();
                    for &ts in &w.stamps {
                        self.metrics.lag(now.saturating_sub(ts) as f64 / 1e6);
                    }
                    Ok((w, by))
                }
                .await;
                if let (Err(_), Some((k, _))) = (&r, &key) {
                    // Not committed (the writer reports a commit as Ok).
                    self.idempotency.finish(k, None);
                }
                // Committed: a retry with its key answers it, whatever its
                // copies say.
                match r {
                    Ok((w, by)) => {
                        if own {
                            self.follows.forget();
                        }
                        self.replicated(w, self.timeout, by).await
                    }
                    Err(e) => Err(e),
                }
            }
            Backend::Cluster(c) => c.write_ops(ops, key).await,
        };
        self.wrote(n, t0, None)
    }

    /// Writes one batch of a stream ([`Self::write_ops`]), which `s` took
    /// ([`Stream::admit`]). A local database's stream waits while the
    /// database is behind on its flushes or the writer's queue is full,
    /// rather than being refused, and reads nothing more from its client
    /// meanwhile. A coordinator's sends a batch a shard refused for its
    /// load (429) again a second later, under the batch's idempotency key,
    /// so that the shards that took it do not apply it twice.
    pub async fn stream_write(self: &Arc<Self>, s: &mut Stream, ops: Vec<Json>) -> Result<Written, ApiError> {
        let give_up = Instant::now() + self.max_timeout;
        let n = s.batches;
        s.batches += 1;
        let w = match &self.backend {
            Backend::Local(db) => {
                let busy = || db.storage().backlogged() || self.writer_state.queued.load(Ordering::Relaxed) >= self.max_queued_writes;
                while busy() && Instant::now() < give_up {
                    tokio::time::sleep(STREAM_POLL).await;
                }
                self.write_ops(ops, None).await?
            }
            Backend::Cluster(_) => {
                let key = Some(format!("stream-{:016x}-{n}", s.id));
                loop {
                    match self.write_ops(ops.clone(), key.clone()).await {
                        Err(e) if e.status == 429 && Instant::now() + RETRY_AFTER < give_up => tokio::time::sleep(RETRY_AFTER).await,
                        r => break r?,
                    }
                }
            }
        };
        s.written += w.rows;
        s.skipped += w.skipped;
        Ok(w)
    }

    /// Waits up to [`BACKLOG_WAIT`] while `db` is behind on its flushes
    /// ([`chloedb::storage::Database::backlogged`]); then refuses the write
    /// (429), so that clients slow down rather than queue.
    async fn backpressure(&self, db: &Db) -> Result<(), ApiError> {
        let until = Instant::now() + BACKLOG_WAIT;
        while db.storage().backlogged() {
            if Instant::now() >= until {
                return Err(ApiError { code: None, status: 429, message: "the database is behind on flushing writes to disk, so the write was not applied; send it again in a second".into() });
            }
            tokio::time::sleep(STREAM_POLL).await;
        }
        Ok(())
    }

    /// The stream sources written from, with the offset of the last event
    /// each wrote: a coordinator's are the least over its shards, where a
    /// stream resumes ([`Coordinator::sources`]).
    pub async fn sources(&self) -> Result<Vec<Source>, ApiError> {
        match &self.backend {
            // Without the marks of the server's own transactions, but those
            // of its state: the cluster it follows, a hold of its writes, a
            // promotion ([`feed::own_mark`]); and at the least offset of
            // each source's slots ([`arrivals::folded`]).
            Backend::Local(db) => {
                let db = db.clone();
                let all = tokio::task::spawn_blocking(move || db.sources()).await.map_err(ApiError::internal)??;
                Ok(arrivals::folded(all, MARK))
            }
            Backend::Cluster(c) => c.sources().await,
        }
    }

    /// Each stream source's offset for the slots moving from this shard,
    /// which the move carries to the shard they go to
    /// ([`arrivals::of_slots`]): asked by the coordinator moving them while
    /// it holds their writes, once their last changes are copied.
    pub async fn moving_offsets(&self) -> Result<Vec<Source>, ApiError> {
        let db = self.local().cloned().ok_or_else(|| ApiError::bad("a coordinator holds no slots"))?;
        let mv = match self.slots() {
            Some((m, shard)) => m.moving().filter(|mv| mv.from == shard).cloned(),
            None => None,
        };
        let mv = mv.ok_or_else(|| conflict("no slots are moving from this shard".into()))?;
        let all = tokio::task::spawn_blocking(move || db.sources()).await.map_err(ApiError::internal)??;
        Ok(arrivals::of_slots(all, &mv.slots, MARK).into_iter().map(|(name, offset)| Source { name, offset, updated: None }).collect())
    }

    /// Each shard's stream sources, in shard order: a server's own alone.
    pub async fn sources_by_shard(&self) -> Result<Vec<Vec<Source>>, ApiError> {
        match &self.backend {
            Backend::Local(_) => Ok(vec![self.sources().await?]),
            Backend::Cluster(c) => c.sources_by_shard().await,
        }
    }

    /// Counts a write's outcome (the request logs a failure on the
    /// server's side), and logs it if it was slow.
    fn wrote(&self, r: Result<Written, ApiError>, t0: Instant, sql: Option<&str>) -> Result<Written, ApiError> {
        let secs = t0.elapsed().as_secs_f64();
        match &r {
            Ok(w) => self.metrics.write(w.rows, secs),
            Err(e) => self.metrics.write_error(e.status),
        }
        if self.slow_after().is_some_and(|s| t0.elapsed() >= s) {
            self.metrics.slow("write");
            let (rows, status) = (r.as_ref().map_or(0, |w| w.rows), r.as_ref().map_or_else(|e| e.status, |_| 200));
            match sql {
                Some(sql) => tracing::warn!(secs, rows, status, sql = %obs::truncate(sql, 2000), "slow write"),
                None => tracing::warn!(secs, rows, status, "slow write"),
            }
        }
        r
    }

    /// Imports rows of `format` from `body` ([`chloedb::ingest`]): upserted
    /// (inserted, into a table without a key) through the writer (or routed to the shards) [`IMPORT_OPS`] rows a
    /// request, so the rows before a failed request stay written. Every
    /// row is read once first, so that a bad one writes nothing; then read
    /// again a request's worth at a time, the next while the last is
    /// written, so that an import holds its body and two requests' rows,
    /// not every row. Returns what was written ([`Written`], its receipt
    /// summed over the requests) and the fields the schema lacks
    /// (skipped). With `opts.strict`, a key twice in the file or one a row
    /// has already is refused before anything is written, and the rows go
    /// as inserts, so that one written meanwhile fails the request too.
    pub async fn import(self: &Arc<Self>, table: Option<&str>, body: axum::body::Bytes, format: Format, opts: ImportOpts) -> Result<(Written, Vec<String>, u64), ApiError> {
        let schema = match table {
            Some(t) => self.table(t).await?,
            None => self.default_table()?,
        };
        // Rows of the first table name none, as before there were tables.
        let named = (table.is_some() || self.first_table().is_placeholder()).then(|| schema.table.clone());
        let strict = opts.strict && schema.primary_key.is_some();
        if strict {
            let (s, b) = (schema.clone(), body.clone());
            let keys = tokio::task::spawn_blocking(move || -> Result<Vec<Value>, ApiError> {
                let pk = s.primary_key.expect("strict needs a key");
                let mut seen: HashSet<String> = HashSet::new();
                let mut keys = Vec::new();
                for (n, d) in Rows::new(&s, std::io::Cursor::new(b), format)?.empty_cells(opts.empty).strict(true).enumerate() {
                    let d = d?;
                    if !seen.insert(js::to_json(&d[pk]).to_string()) {
                        return Err(conflict(format!("strict: row {} repeats key {} of an earlier row; nothing was written", n + 1, key_text(&d[pk]))));
                    }
                    keys.push(d[pk].clone());
                }
                Ok(keys)
            })
            .await
            .map_err(ApiError::internal)??;
            for chunk in keys.chunks(MAX_GET_KEYS) {
                let got = self.get_rows(Some(&schema.table), chunk.to_vec(), None, &After::default(), 0, false).await?;
                let there: Vec<&Value> = chunk.iter().zip(&got.rows).filter(|(_, r)| r.is_some()).map(|(k, _)| k).collect();
                if let Some(k) = there.first() {
                    return Err(conflict(format!("strict: {} has a row with key {} already ({} of the file's keys are there); nothing was written", schema.table, key_text(k), there.len())));
                }
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chloedb::Result<Vec<Json>>>(1);
        let reader = tokio::task::spawn_blocking(move || -> chloedb::Result<(Vec<String>, u64)> {
            let mut check = Rows::new(&schema, std::io::Cursor::new(body.clone()), format)?.empty_cells(opts.empty).strict(opts.strict);
            check.by_ref().try_for_each(|d| d.map(drop))?;
            let mut rows = Rows::new(&schema, std::io::Cursor::new(body), format)?.empty_cells(opts.empty).strict(opts.strict);
            // A strict import inserts; a table without a key (log lines)
            // takes its rows as inserts too.
            let kind = if strict || schema.primary_key.is_none() { "insert" } else { "upsert" };
            loop {
                let op = |d: chloedb::value::Document| {
                    let mut op = json!({"op": kind, "doc": js::document_json(&schema, &d)});
                    if let Some(t) = &named {
                        op["table"] = json!(t);
                    }
                    op
                };
                let chunk: chloedb::Result<Vec<Json>> = rows.by_ref().take(IMPORT_OPS).map(|d| d.map(op)).collect();
                let last = chunk.as_ref().map_or(true, |c| c.len() < IMPORT_OPS);
                // The writing side stopped: a write failed.
                if tx.blocking_send(chunk).is_err() || last {
                    break;
                }
            }
            Ok((rows.unknown.into_iter().collect(), rows.ragged))
        });
        let mut all = Written { receipt: Some(Receipt::default()), ..Default::default() };
        while let Some(chunk) = rx.recv().await {
            let ops = chunk?;
            if !ops.is_empty() {
                let w = self.write_ops(ops, None).await?;
                all.add(w);
            }
        }
        let (unknown, ragged) = reader.await.map_err(ApiError::internal)??;
        Ok((all, unknown, ragged))
    }

    /// Imports the tables of the SQLite file in `body`
    /// ([`chloedb::sqlite`]): each into the table of its name, created from
    /// the SQLite table when the database lacks it, its rows upserted
    /// through the writer (or routed to the shards) [`IMPORT_OPS`] a
    /// request. The plan is made and every row read once before anything
    /// is written, so that a value that does not fit writes nothing. With
    /// `plan_only`, returns the plan ([`chloedb::sqlite::Plan::to_json`])
    /// and writes nothing; otherwise what was written
    /// ([`chloedb::sqlite::Imported::to_json`]).
    /// Rows need a write key, and an import that creates tables an admin
    /// key, refused before anything is written.
    pub async fn import_sqlite(self: &Arc<Self>, who: &Principal, body: axum::body::Bytes, opts: chloedb::sqlite::ImportOptions, plan_only: bool, timeout_ms: u64) -> Result<Json, ApiError> {
        who.may(Scope::Write, "an import")?;
        let catalog = self.catalog();
        let checked = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
            let dir = tempfile::tempdir().map_err(ApiError::internal)?;
            let path = dir.path().join("import.sqlite");
            std::fs::write(&path, &body).map_err(ApiError::internal)?;
            // The server's temporary path is no business of the client's.
            let shown = path.display().to_string();
            let src = chloedb::sqlite::Source::open(&path).map_err(|e| ApiError::bad(e.to_string().replace(&shown, "the uploaded file")))?;
            let plan = src.plan_over(&catalog, &opts)?;
            if !plan_only {
                for t in &plan.tables {
                    src.documents(t, |docs| {
                        for d in docs {
                            d?;
                        }
                        Ok(())
                    })?;
                }
            }
            Ok((dir, src, plan))
        })
        .await
        .map_err(ApiError::internal)??;
        let (dir, src, plan) = checked;
        if plan_only {
            return Ok(plan.to_json());
        }
        if who.limited() {
            for t in &plan.tables {
                who.may_table(if t.exists { Scope::Write } else { Scope::Admin }, &t.schema.table, "an import")?;
            }
        }
        let creates: Vec<&str> = plan.tables.iter().filter(|t| !t.exists).map(|t| t.schema.table.as_str()).collect();
        if !creates.is_empty() {
            who.may(Scope::Admin, &format!("an import that creates tables ({})", creates.join(", ")))?;
            keys::note(format!("SQLite import created tables {}", creates.join(", ")));
        }
        enum Step {
            Table(usize),
            Ops(Vec<Json>),
        }
        let tables = plan.tables.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chloedb::Result<Step>>(1);
        let reader = tokio::task::spawn_blocking(move || {
            let _dir = dir;
            for (i, t) in tables.iter().enumerate() {
                if tx.blocking_send(Ok(Step::Table(i))).is_err() {
                    return;
                }
                let op = if t.schema.primary_key.is_some() { "upsert" } else { "insert" };
                let r = src.documents(t, |docs| {
                    loop {
                        let chunk: chloedb::Result<Vec<Json>> =
                            (&mut *docs).take(IMPORT_OPS).map(|d| d.map(|d| json!({"op": op, "table": t.schema.table, "doc": js::document_json(&t.schema, &d)}))).collect();
                        let last = chunk.as_ref().map_or(true, |c| c.len() < IMPORT_OPS);
                        // The writing side stopped: a write failed.
                        if tx.blocking_send(chunk.map(Step::Ops)).is_err() {
                            return Err(chloedb::Error::Query("stopped".into()));
                        }
                        if last {
                            return Ok(());
                        }
                    }
                });
                if r.is_err() {
                    return;
                }
            }
        });
        let mut out = chloedb::sqlite::Imported { tables: Vec::new(), skipped: plan.skipped.clone() };
        while let Some(step) = rx.recv().await {
            match step? {
                Step::Table(i) => {
                    let t = &plan.tables[i];
                    if !t.exists {
                        self.create_table(t.schema.clone(), timeout_ms).await?;
                    }
                    out.tables.push(chloedb::sqlite::TableImport { source: t.source.clone(), table: t.schema.table.clone(), rows: 0, created: !t.exists, bulk: false, notes: t.notes.clone() });
                }
                Step::Ops(ops) => {
                    if !ops.is_empty() {
                        let n = self.write_ops(ops, None).await?.rows;
                        if let Some(t) = out.tables.last_mut() {
                            t.rows += n;
                        }
                    }
                }
            }
        }
        reader.await.map_err(ApiError::internal)?;
        Ok(out.to_json())
    }

    /// Imports the Parquet file `body` into one table, as
    /// [`Self::import_sqlite`] imports a SQLite file's: into the table
    /// `opts.table` names (else the one an export kept in the file),
    /// created from the file's columns when missing
    /// ([`chloedb::parquet::Source::plan_over`]), the rows checked before any
    /// is written and then written through the writer (or routed to the
    /// shards) [`IMPORT_OPS`] a request. With `plan_only`, returns the plan
    /// ([`chloedb::parquet::Plan::to_json`]) and writes nothing; otherwise
    /// what was written ([`chloedb::parquet::Imported::to_json`]). Rows need
    /// a write key, and an import that creates the table an admin key.
    #[cfg(feature = "parquet")]
    pub async fn import_parquet(self: &Arc<Self>, who: &Principal, body: axum::body::Bytes, opts: chloedb::parquet::ImportOptions, plan_only: bool, timeout_ms: u64) -> Result<Json, ApiError> {
        who.may(Scope::Write, "an import")?;
        let catalog = self.catalog();
        let checked = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
            let dir = tempfile::tempdir().map_err(ApiError::internal)?;
            let path = dir.path().join("upload.parquet");
            std::fs::write(&path, &body).map_err(ApiError::internal)?;
            let shown = path.display().to_string();
            let hide = |e: chloedb::Error| ApiError::bad(e.to_string().replace(&shown, "the uploaded file").replace("upload.parquet: ", ""));
            let src = chloedb::parquet::Source::open(&path).map_err(hide)?;
            if opts.table.is_none() && opts.schema.is_none() && src.kept_table().is_none() {
                return Err(ApiError::bad("name the table the rows go to (?table=): the file keeps none"));
            }
            let plan = src.plan_over(&catalog, &opts).map_err(hide)?;
            if !plan_only {
                src.documents(&plan, |docs| {
                    for d in docs {
                        d?;
                    }
                    Ok(())
                })
                .map_err(hide)?;
            }
            Ok((dir, src, plan))
        })
        .await
        .map_err(ApiError::internal)??;
        let (dir, src, plan) = checked;
        if plan_only {
            return Ok(plan.to_json());
        }
        if who.limited() {
            who.may_table(if plan.exists { Scope::Write } else { Scope::Admin }, &plan.schema.table, "an import")?;
        }
        if !plan.exists {
            who.may(Scope::Admin, &format!("an import that creates a table ({})", plan.schema.table))?;
            keys::note(format!("Parquet import created table {}", plan.schema.table));
            self.create_table(plan.schema.clone(), timeout_ms).await?;
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chloedb::Result<Vec<Json>>>(1);
        let p = plan.clone();
        let reader = tokio::task::spawn_blocking(move || {
            let _dir = dir;
            let op = if p.schema.primary_key.is_some() { "upsert" } else { "insert" };
            src.documents(&p, |docs| loop {
                let chunk: chloedb::Result<Vec<Json>> = (&mut *docs).take(IMPORT_OPS).map(|d| d.map(|d| json!({"op": op, "table": p.schema.table, "doc": js::document_json(&p.schema, &d)}))).collect();
                let last = chunk.as_ref().map_or(true, |c| c.len() < IMPORT_OPS);
                if tx.blocking_send(chunk).is_err() {
                    return Err(chloedb::Error::Query("stopped".into()));
                }
                if last {
                    return Ok(());
                }
            })
            .map(|((), nonfinite)| nonfinite)
        });
        let mut out = chloedb::parquet::Imported { table: plan.schema.table.clone(), files: plan.files, created: !plan.exists, skipped: plan.skipped.clone(), notes: plan.notes.clone(), ..Default::default() };
        while let Some(ops) = rx.recv().await {
            let ops = ops?;
            if !ops.is_empty() {
                out.rows += self.write_ops(ops, None).await?.rows;
            }
        }
        if let Ok(Ok(nonfinite)) = reader.await {
            out.notes.extend(chloedb::parquet::nonfinite_notes(&plan.schema, &nonfinite));
        }
        Ok(out.to_json())
    }

    /// Writes one table (the server's only one, or `table`) to a Parquet
    /// file, from one snapshot ([`chloedb::parquet::export`]), as
    /// [`Self::export_sqlite`] writes a SQLite file.
    #[cfg(feature = "parquet")]
    pub async fn export_parquet(self: &Arc<Self>, table: Option<String>, meta: bool) -> Result<(tempfile::TempDir, PathBuf, chloedb::parquet::Exported), ApiError> {
        let Backend::Local(db) = &self.backend else {
            return Err(ApiError::bad("a coordinator holds no rows: export from each shard's server, or with chloedb-server export"));
        };
        let db = db.clone();
        tokio::task::spawn_blocking(move || {
            let table = match table {
                Some(t) => t,
                None => {
                    let all = db.catalog();
                    match all.as_slice() {
                        [one] => one.table.clone(),
                        _ => return Err(ApiError::bad(format!("a Parquet file holds one table: name it (?tables=), one of {}", all.iter().map(|s| s.table.as_str()).collect::<Vec<_>>().join(", ")))),
                    }
                }
            };
            let dir = tempfile::tempdir().map_err(ApiError::internal)?;
            let path = dir.path().join("export.parquet");
            let r = chloedb::parquet::export(&db, &path, &chloedb::parquet::ExportOptions { tables: vec![table], meta })?;
            Ok((dir, path, r))
        })
        .await
        .map_err(ApiError::internal)?
    }

    /// Creates a table, as `CREATE TABLE` does ([`Self::ddl`]): on a local
    /// database, once no write holds the writer; a primary ships it with
    /// the rows. A coordinator creates tables only through `CREATE TABLE`.
    async fn create_table(self: &Arc<Self>, schema: Schema, timeout_ms: u64) -> Result<(), ApiError> {
        let db = match &self.backend {
            Backend::Local(db) => db,
            // On every shard, as CREATE TABLE through the coordinator, so
            // that an import makes the table its plan says it makes.
            Backend::Cluster(_) => {
                let sql = chloedb::query::ddl::create_sql(&schema);
                self.ddl(sql.trim_end_matches(';'), &Params::new(), timeout_ms, "import").await?;
                return Ok(());
            }
        };
        self.takes_writes()?;
        let _entry = self.moving.enter();
        self.not_held()?;
        let deadline = Instant::now() + self.timeout_of(timeout_ms);
        let db = db.clone();
        tokio::task::spawn_blocking(move || wait_for_writer(deadline, || Ok(db.create_table(schema.clone())?))).await.map_err(ApiError::internal)??;
        self.refresh_models();
        Ok(())
    }

    /// Writes the tables (all, or `opts.tables`) to a new SQLite file, from
    /// one snapshot ([`chloedb::sqlite::export`]), in a directory that is
    /// removed when the returned [`tempfile::TempDir`] is dropped. Only a
    /// server with the database: a coordinator holds no rows.
    pub async fn export_sqlite(self: &Arc<Self>, opts: chloedb::sqlite::ExportOptions) -> Result<(tempfile::TempDir, PathBuf, chloedb::sqlite::Exported), ApiError> {
        let Backend::Local(db) = &self.backend else {
            return Err(ApiError::bad("a coordinator holds no rows: export from each shard's server, or with chloedb-server export"));
        };
        let db = db.clone();
        tokio::task::spawn_blocking(move || {
            let dir = tempfile::tempdir().map_err(ApiError::internal)?;
            let path = dir.path().join("export.sqlite");
            let r = chloedb::sqlite::export(&db, &path, &opts)?;
            Ok((dir, path, r))
        })
        .await
        .map_err(ApiError::internal)?
    }

    /// "ok"; for a database, "failing" when it cannot write until it
    /// restarts ([`Self::failing`]) and "degraded" while its flushes fail
    /// (writes still commit, to the WAL, until the memtable holds twice its
    /// limit); for a coordinator, its shards' state ([`Coordinator::health`]).
    pub async fn health(self: &Arc<Self>) -> Json {
        match &self.backend {
            Backend::Local(db) => {
                let s = db.snapshot();
                let mut h = json!({"status": "ok", "role": "shard", "docs": s.n_live_docs(), "segments": s.segments().len(), "txn": s.txn()});
                // cluster2: the rows of every table (`docs` counts the first
                // only), which a move or a new copy sends: plans and moves
                // count these.
                let tables: serde_json::Map<String, Json> = s.schemas().filter(|t| t.table != SOURCES && t.table != FUNCTIONS).filter_map(|t| s.table(&t.table).map(|v| (t.table.clone(), json!(v.n_live_docs())))).collect();
                h["rows"] = json!(tables.values().filter_map(Json::as_u64).sum::<u64>());
                h["tables"] = Json::Object(tables);
                if let Some((m, shard)) = self.slots() {
                    h["slots"] = json!({"epoch": m.epoch, "shard": shard, "of": m.shards()});
                }
                match &self.replication() {
                    Replication::None => {}
                    Replication::Primary(p) => h["replication"] = p.health(),
                    Replication::Replica(f) => h["replication"] = f.health().await,
                }
                if let Some(why) = self.failing() {
                    h["status"] = json!("failing");
                    h["error"] = json!(why);
                } else if let Some(e) = db.storage().flush_error() {
                    h["status"] = json!("degraded");
                    h["error"] = json!(format!("flushes fail: {e}"));
                } else if let Some(what) = db.storage().damaged() {
                    // Opened past damage (--recover, --wal-drop-damaged):
                    // it serves what was left, which is not all it held.
                    h["status"] = json!("degraded");
                    h["error"] = json!(format!("opened past damage: {what}"));
                } else if let Some(at) = match &self.replication() {
                    Replication::Replica(f) => f.differs(),
                    _ => None,
                } {
                    // It serves no reads, and waits for a copy of its
                    // primary.
                    h["status"] = json!("degraded");
                    h["error"] = json!(format!(
                        "this replica holds another transaction than its primary's as transaction {} of term {}: it applies nothing and waits for a copy of the primary",
                        at.seq, at.term
                    ));
                }
                if let Some(what) = db.storage().damaged() {
                    h["opened_past_damage"] = json!(what);
                }
                // What the last scrub found: the copy still serves (a query
                // that reads a damaged section fails, and a coordinator
                // tries another copy), but it is to be replaced.
                h["scrub"] = self.scrubs.json();
                let damaged = self.scrubs.damaged();
                if !damaged.is_empty() {
                    h["damaged"] = json!(damaged);
                }
                h
            }
            Backend::Cluster(c) => c.health().await,
        }
    }

    /// Why this server cannot write until it restarts, if it cannot: a
    /// write failed partway (a full disk, an I/O error), after which the
    /// database refuses every write until it is reopened, or the writer
    /// thread stopped. Liveness (`/livez`) fails then, so that the server
    /// is restarted. A coordinator is never failing; its shards may be.
    pub fn failing(&self) -> Option<String> {
        match &self.backend {
            Backend::Local(db) if db.storage().is_poisoned() => Some("a write failed partway (a full disk, an I/O error), so the database refuses writes until it is reopened: fix the cause and restart the server, a primary once another copy is the primary".into()),
            Backend::Local(_) if !self.writer_state.alive.load(Ordering::Relaxed) => Some("the writer thread stopped: restart the server".into()),
            _ => None,
        }
    }

    /// Storage statistics and what the merges are doing (local), and
    /// query counters.
    pub async fn stats(self: &Arc<Self>) -> Json {
        let (storage, merges) = match &self.backend {
            Backend::Local(db) => (
                serde_json::to_value(db.storage().file_stats()).unwrap_or(Json::Null),
                serde_json::to_value(db.storage().merge_status()).unwrap_or(Json::Null),
            ),
            Backend::Cluster(_) => (Json::Null, Json::Null),
        };
        json!({"health": self.health().await, "storage": storage, "merges": merges, "metrics": self.metrics.summary()})
    }

    /// The counters and gauges of this moment, for the series
    /// ([`telemetry::Series`]). Reads no shard: a coordinator's own.
    pub fn sample(&self) -> Sample {
        let mut s = Sample { at: Some(Instant::now()), ts_ms: telemetry::now_ms(), ..Default::default() };
        self.metrics.totals(&mut s);
        s.query_max_s = self.metrics.take_query_max();
        let (rss, cpu) = telemetry::process_usage();
        s.cpu_s = cpu;
        let heap = crate::heap::stats();
        (s.alloc_bytes, s.allocs) = (heap.allocated_bytes, heap.allocs);
        let g = &mut s.gauges;
        if let Some(r) = rss {
            g.insert("rss_bytes", r as f64);
        }
        if heap.installed {
            g.insert("heap_bytes", heap.in_use_bytes as f64);
            g.insert("heap_peak_bytes", heap.peak_bytes as f64);
        }
        g.insert("running", self.running() as f64);
        g.insert("live", self.live.len() as f64);
        // A cluster that follows another: its follow processes' lag.
        let lags = self.follow_lags();
        if !lags.is_empty() {
            g.insert("follow_lag_s", lags.iter().map(|l| l.1).fold(0.0, f64::max));
            g.insert("follow_behind", lags.iter().map(|l| l.2).fold(0.0, f64::max));
        }
        g.insert("slots_free", self.admission.available_permits() as f64);
        if let Some(b) = &self.query_memory {
            g.insert("query_memory_bytes", b.used() as f64);
            g.insert("query_memory_limit_bytes", b.limit() as f64);
            if b.limit() > 0 {
                g.insert("query_memory_ratio", b.used() as f64 / b.limit() as f64);
            }
        }
        if let Backend::Local(db) = &self.backend {
            let snap = db.snapshot();
            let f = db.storage().file_stats();
            let m = db.storage().merge_status();
            let deleted: u64 = f.segments.iter().map(|s| s.n_deleted as u64).sum::<u64>() + f.pending_deletes as u64;
            for (k, v) in [
                ("docs", snap.n_live_docs() as f64),
                ("segments", snap.segments().len() as f64),
                ("deleted_docs", deleted as f64),
                ("memtable_docs", f.memtable_docs as f64),
                ("file_bytes", f.file_bytes as f64),
                ("wal_bytes", f.wal_bytes as f64),
                ("reusable_bytes", (f.free_pages * chloedb::storage::pager::PAGE) as f64),
                ("merges_running", m.running as f64),
                ("merge_memory_bytes", m.memory as f64),
                ("write_queue", self.writer_state.queued.load(Ordering::Relaxed) as f64),
                ("leases", self.leases.lock().unwrap_or_else(|e| e.into_inner()).len() as f64),
                ("failing", self.failing().is_some() as u8 as f64),
                ("flush_failing", db.storage().flush_error().is_some() as u8 as f64),
                ("merge_failing", m.error.is_some() as u8 as f64),
                ("damaged_segments", self.scrubs.damaged().len() as f64),
                ("pool_queued", chloedb::query::pool::Pool::global().queued() as f64),
            ] {
                g.insert(k, v);
            }
            if let Some((total, free, avail)) = telemetry::disk(db.storage().path()) {
                g.insert("disk_total_bytes", total as f64);
                g.insert("disk_free_bytes", avail as f64);
                if let Some(r) = telemetry::disk_used_ratio(total, free, avail) {
                    g.insert("disk_used_ratio", r);
                }
            }
            match &self.replication() {
                Replication::None => {}
                Replication::Primary(p) => {
                    let h = p.health();
                    let rs = h["replicas"].as_array().cloned().unwrap_or_default();
                    g.insert("replicas", rs.len() as f64);
                    let in_sync = rs.iter().filter(|r| r["in_sync"] == true).count();
                    g.insert("replicas_in_sync", in_sync as f64);
                    g.insert("replicas_out_of_sync", (rs.len() - in_sync) as f64);
                    g.insert("replica_behind_max", rs.iter().filter_map(|r| r["behind"].as_f64()).fold(0.0, f64::max));
                    // Of the replicas that apply this primary's log: not one
                    // being sent a whole copy (it is behind by all of it until
                    // the copy is in), nor one not heard since this primary
                    // started (its number is not known yet): the lag alert's.
                    let applying = |r: &&Json| r["copying"] != true && r["member"] != false && r["answered"] != false;
                    g.insert("replica_behind_applying", rs.iter().filter(applying).filter_map(|r| r["behind"].as_f64()).fold(0.0, f64::max));
                    g.insert("seq_no", h["seq_no"].as_f64().unwrap_or(0.0));
                }
                Replication::Replica(f) => {
                    g.insert("seq_no", db.storage().numbering().map_or(0, |n| n.seq) as f64);
                    g.insert("serves_reads", f.serves_reads() as u8 as f64);
                }
            }
        }
        s
    }

    /// The process, its limits and where its database is
    /// (`GET /v1/system`).
    pub fn system(&self) -> Json {
        let (rss, cpu) = telemetry::process_usage();
        let started = self.started.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let uptime = self.started.elapsed().map_or(0.0, |d| d.as_secs_f64());
        let db = match &self.backend {
            Backend::Local(db) => {
                let path = db.storage().path();
                let disk = telemetry::disk(path).map(|(total, free, avail)| json!({"total_bytes": total, "free_bytes": free, "available_bytes": avail}));
                json!({"path": path.display().to_string(), "disk": disk, "tables": self.catalog().len(), "seq_no": db.storage().numbering().map(|n| json!({"term": n.term, "seq": n.seq}))})
            }
            Backend::Cluster(_) => Json::Null,
        };
        json!({
            "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id(), "started_ms": started, "uptime_s": uptime,
            "cores": self.cores, "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "simd": chloedb::simd::describe(),
            "rss_bytes": rss, "cpu_seconds": cpu, "memory_limit_bytes": chloedb::storage::memory_limit(),
            "backend": match self.backend { Backend::Local(_) => "local", Backend::Cluster(_) => "coordinator" },
            "role": self.role().as_str_name().to_lowercase(),
            "db": db,
            "limits": {
                "timeout_ms": self.timeout.as_millis() as u64, "max_timeout_ms": self.max_timeout.as_millis() as u64,
                "max_concurrent": self.max_concurrent, "max_rows": self.max_rows, "max_groups": self.max_groups,
                "query_memory_bytes": self.query_memory.as_ref().map(|b| b.limit()), "max_queued_writes": self.max_queued_writes,
                "max_body_bytes": self.max_body, "slow_ms": self.slow_after().map(|d| d.as_millis() as u64), "lease_ttl_s": self.lease_ttl.as_secs(),
            },
            "features": {
                "synonyms": self.synonyms().map_or(0, |s| s.rules()), "backup_dir": self.backup_dir.as_ref().map(|p| p.display().to_string()),
                "models": self.models().len(), "agent_key": self.agent.api_key.is_some(),
            },
            "sampling": {"interval_ms": self.series.interval.as_millis() as u64, "points": telemetry::POINTS},
        })
    }

    /// A profile of `node`, a server of this coordinator's cluster
    /// ([`Coordinator::profile`]); a server that is not a coordinator has
    /// no other node to profile.
    pub async fn profile_node(&self, node: &str, a: crate::profiling::Asked) -> Result<crate::profiling::Answer, ApiError> {
        match &self.backend {
            Backend::Local(_) => Err(ApiError::bad("node names a server of a coordinator's cluster: this server is not a coordinator, so leave node out to profile it")),
            Backend::Cluster(c) => c.profile(node, a).await,
        }
    }

    /// Where this server is in its cluster, and how each copy is doing
    /// (`GET /v1/cluster`): a server's own health and latest point; a
    /// coordinator asks every replica of every shard
    /// ([`Coordinator::cluster`]).
    pub async fn cluster(self: &Arc<Self>) -> Json {
        let mut j = match &self.backend {
            Backend::Local(_) => json!({"role": self.role().as_str_name().to_lowercase(), "node": self.detail().await, "at_ms": telemetry::now_ms()}),
            Backend::Cluster(c) => {
                let mut j = (*c.snapshot().await).clone();
                j["coordinator"] = json!({"load": self.series.last().map(|p| (*p).clone()), "health": {"retries": j["retries"], "hedges": j["hedges"]}, "version": self.place.version(), "name": self.place.name, "node": self.place.node});
                j
            }
        };
        j["cluster"] = self.identity.json();
        j
    }

    /// Keeps an audit entry ([`Audit::add`]).
    pub fn audit_add(&self, e: keys::Entry) {
        self.audit_keep(e, true);
    }

    /// Keeps an audit entry; a change is a `change` event too when
    /// `event`, which is false when its request made its own event of it
    /// (an alert acknowledged, a log level set: alerts agent).
    pub fn audit_keep(&self, e: keys::Entry, event: bool) {
        if let Some(s) = &self.log_store {
            s.keep_audit(&e);
        }
        if event && e.kind() == keys::CHANGE {
            self.events.add(crate::events::of_change(&e));
        }
        self.audit.add(e);
    }

    /// What this server can do for its shard now: serve reads and take
    /// writes, and why not (a coordinator reads it from each copy's
    /// detail, [`Coordinator::cluster`]). A primary serves no reads
    /// without a majority of its copies (with failover), and takes no
    /// writes without enough of them in sync; a replica takes none from
    /// clients.
    pub fn capability(&self) -> Json {
        let Backend::Local(_) = &self.backend else { return Json::Null };
        let (reads, writes): (Result<(), String>, Result<(), String>) = match self.replication() {
            Replication::None => (Ok(()), Ok(())),
            // Whether it would take a write now: the write's flight ends at once.
            Replication::Primary(p) => (p.readable().map_err(|e| e.message), p.admit().map(drop).map_err(|e| e.message)),
            Replication::Replica(f) => (
                match f.serves_reads() {
                    true => Ok(()),
                    false => Err("this replica serves no reads while it catches up with its primary, or has not heard from it".into()),
                },
                Err("a replica takes no writes from clients".into()),
            ),
        };
        let writes = match self.failing() {
            Some(why) => Err(why),
            None => writes,
        };
        let mut j = json!({"serves_reads": reads.is_ok(), "takes_writes": writes.is_ok(), "failover_after_ms": self.failover_after.as_millis() as u64});
        if let Err(e) = reads {
            j["no_reads"] = json!(e);
        }
        if let Err(e) = writes {
            j["no_writes"] = json!(e);
        }
        j
    }

    /// Sets the log filter (`debug`, `info,chloedb_server::coord=debug`)
    /// for `expires_s` seconds ([`LOG_FILTER_S`] by default; 0: until it
    /// is set again), then back to the one before; each change, and the
    /// way back, is an event (`log_level`).
    pub fn set_log_level(&self, filter: &str, expires_s: Option<u64>, by: &str) -> Result<Json, ApiError> {
        let before = crate::obs::log_filter();
        crate::obs::set_log_filter(filter).map_err(ApiError::bad)?;
        let now = telemetry::now_ms();
        let secs = expires_s.unwrap_or(LOG_FILTER_S);
        let expires_ms = (secs > 0).then(|| now + secs * 1000);
        let generation = {
            let mut l = self.log_level.lock().unwrap_or_else(|e| e.into_inner());
            // The filter to go back to is the one before the first of a run
            // of changes that expire.
            let previous = match (&l.expires_ms, expires_ms) {
                (Some(_), Some(_)) => l.previous.clone(),
                (_, Some(_)) => before.clone(),
                (_, None) => None,
            };
            *l = LogLevel { previous, set_by: Some(by.to_string()), set_ms: Some(now), expires_ms, generation: l.generation + 1 };
            l.generation
        };
        tracing::info!(filter, by, expires_s = secs, "log filter changed");
        let until = expires_ms.map_or(String::new(), |_| format!(" for {} min", secs.div_ceil(60)));
        self.events.add(crate::events::Event {
            kind: "log_level".into(),
            severity: "info".into(),
            title: format!("Logs at {filter}{until}"),
            detail: json!({"filter": filter, "before": before, "expires_ms": expires_ms}),
            where_: Json::Null,
            who: Some(by.to_string()),
        });
        if let Some(at) = expires_ms {
            let (level, events) = (self.log_level.clone(), self.events.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(at.saturating_sub(telemetry::now_ms()))).await;
                let mut l = level.lock().unwrap_or_else(|e| e.into_inner());
                if l.generation != generation {
                    return;
                }
                let back = l.previous.clone().unwrap_or_else(|| "info".into());
                if let Err(e) = crate::obs::set_log_filter(&back) {
                    tracing::warn!(error = %e, "the log filter could not go back");
                    return;
                }
                *l = LogLevel { generation: l.generation + 1, ..LogLevel::default() };
                drop(l);
                tracing::info!(filter = %back, "log filter back, its time over");
                events.add(crate::events::Event { kind: "log_level".into(), severity: "info".into(), title: format!("Logs back at {back}: the change expired"), detail: json!({"filter": back, "expired": true}), where_: Json::Null, who: None });
            });
        }
        Ok(self.log_filter_json())
    }

    /// The log filter, and until when and by whom it was set.
    pub fn log_filter_json(&self) -> Json {
        let l = self.log_level.lock().unwrap_or_else(|e| e.into_inner()).clone();
        json!({"filter": crate::obs::log_filter(), "expires_ms": l.expires_ms, "set_by": l.set_by, "set_ms": l.set_ms, "previous": l.previous})
    }

    /// Fleet health (`GET /v1/health/fleet`, [`crate::fleet`]): of the
    /// cluster on a coordinator, of this server on a shard server.
    pub async fn fleet_health(self: &Arc<Self>) -> Json {
        let now = telemetry::now_ms();
        let (cluster, partial, coordinator) = match &self.backend {
            Backend::Cluster(c) => {
                let me = match crate::peers::this(self) {
                    Some(p) => json!({"url": p.url, "name": p.name}),
                    None => json!({"url": self.identity.public_url, "name": self.place.name.clone().or_else(|| self.place.node.clone())}),
                };
                ((*c.snapshot().await).clone(), c.opts.partial, Some(me))
            }
            Backend::Local(_) => (crate::fleet::local_shard(&self.detail().await), false, None),
        };
        let history = self.series.since(now.saturating_sub(3_600_000));
        let from = now.saturating_sub(crate::fleet::WINDOW_S * 1000);
        let points: Vec<Arc<Json>> = history.iter().filter(|p| p["t"].as_u64().unwrap_or(0) > from).cloned().collect();
        // fleet: the alerts firing that nobody silenced, each rule once, as the bell counts them
        // (alerts2: nor a cause holds back).
        let alerts: Vec<Json> = self.alerts.json()["states"].as_array().map_or_else(Vec::new, |v| v.iter().filter(|s| s["state"] == "firing" && s["silenced"].is_null() && s["held_by"].is_null()).cloned().collect());
        self.fleet.judge(crate::fleet::Inputs {
            cluster: &cluster,
            points: &points,
            history: &history,
            slo_error_ratio: self.slo.error_ratio,
            slo_p99_ms: self.slo.p99_ms,
            alerts,
            partial,
            coordinator,
            peers: crate::peers::windows(self),
            gone: if matches!(self.backend, Backend::Cluster(_)) { crate::peers::gone(self) } else { Vec::new() },
            now_ms: now,
        })
    }

    /// This server's health and latest point, as a coordinator's
    /// `/v1/cluster` shows each shard's copy, and its last minute's load
    /// (`minute`), which autoscaling reads.
    pub async fn detail(self: &Arc<Self>) -> Json {
        let health = self.health().await;
        let load = self.series.last().map(|p| (*p).clone());
        let log_store = self.log_store.as_ref().map(|s| s.status());
        let minute = self.minute_load(telemetry::now_ms()).map(|l| {
            json!({"qps": l.qps, "cpu": l.cpu, "cores": l.cores, "busy": l.busy(), "p99_ms": l.p99_ms, "disk_used_ratio": l.disk_used_ratio})
        });
        json!({
            "health": health, "load": load, "minute": minute,
            // The Simulator reads `cpu_seconds` (the process's CPU so far) before and after a test.
            "system": {"version": env!("CARGO_PKG_VERSION"), "uptime_s": self.started.elapsed().map_or(0.0, |d| d.as_secs_f64()), "cores": self.cores, "cpu_seconds": telemetry::process_usage().1, "memory_limit_bytes": chloedb::storage::memory_limit()},
            "running": self.live.len(), "log_store": log_store,
            // core: what a coordinator's /v1/cluster shows of each copy.
            "capability": self.capability(),
            // cluster2: the flags it runs with, for Settings (NEW-cluster-admin-5).
            "limits": {"slow_ms": self.slow_after().map(|d| d.as_millis() as u64), "max_concurrent": self.max_concurrent, "max_rows": self.max_rows, "timeout_ms": self.timeout.as_millis() as u64},
            "place": {"name": self.place.name, "node": self.place.node, "zone": self.place.zone},
            "version": self.place.version(),
            "backup_dir": self.backup_dir.as_ref().map(|p| p.display().to_string()),
            "cluster": self.identity.json(),
            // alerts agent: the alerts firing here, and this copy's states of
            // the each_copy rules, which a coordinator gathers.
            "alerts": self.alerts.summary(),
            // fleet: the coordinators that named themselves here ([`crate::peers`]).
            "coordinators": self.peers.heard_json(),
        })
    }

    /// This server's load over the minute before `now_ms` (from its
    /// points), as the log store's `load` keeps it.
    pub fn minute_load(&self, now_ms: u64) -> Option<crate::logstore::Load> {
        let points = self.series.since(now_ms.saturating_sub(60_000));
        let (qps, cpu, p99_ms, disk_used_ratio) = telemetry::load_of(&points)?;
        let role = match &self.backend {
            Backend::Cluster(_) => "coordinator",
            Backend::Local(_) => match self.role() {
                pb::Role::Primary => "primary",
                pb::Role::Replica => "replica",
                _ => "standalone",
            },
        };
        let shard = self.slots().map(|(_, s)| s);
        // core: what the command center charts past the hour in memory.
        let w = crate::fleet::Window::of(&points);
        let secs = w.secs.max(1e-3);
        let per_s = |k: &str, rate: &str| {
            let n: Option<f64> = points.iter().map(|p| p[k].as_f64().or_else(|| p[rate].as_f64().map(|r| r * p["secs"].as_f64().unwrap_or(2.0)))).sum();
            n.map(|n| n / secs)
        };
        let mean = |k: &str| {
            let v: Vec<f64> = points.iter().filter_map(|p| p[k].as_f64()).collect();
            (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
        };
        let p50_ms = (w.hist.iter().sum::<u64>() > 0).then(|| telemetry::quantile(&crate::metrics::BUCKETS, &w.hist, 0.5).map(|s| s * 1e3)).flatten();
        Some(crate::logstore::Load {
            ts_ms: now_ms,
            role: role.into(),
            shard,
            qps,
            cpu,
            cores: self.cores,
            p99_ms,
            disk_used_ratio,
            error_ratio: w.error_ratio(),
            errors_s: Some(w.failures / secs),
            client_errors_s: per_s("client_errors", "http_4xx_s"),
            timeouts_s: per_s("timeouts", "timeouts_s"),
            rejected_s: mean("rejected_s"),
            p50_ms,
            writes_s: mean("writes_s"),
            mem_bytes: points.iter().rev().find_map(|p| p["rss_bytes"].as_u64()),
        })
    }

    /// The metrics (`GET /metrics`). A coordinator does not ask its shards
    /// (each serves its own); it shows how its calls to them went.
    pub async fn prometheus(self: &Arc<Self>) -> String {
        let one = Family::one;
        let mut more = vec![
            one("chloedb_queries_running", "gauge", "Queries running now, those whose callers stopped waiting included.", self.running() as f64),
            Family { name: "chloedb_alerts_firing", ty: "gauge", help: "Alerts firing now, by severity (GET /v1/alerts).", values: self.alerts.firing() },
            one("chloedb_api_keys", "gauge", "API keys the server holds (GET /v1/admin/keys).", self.keys.list().len() as f64),
            one(
                "chloedb_connections_closed_past_share_total",
                "counter",
                "Connections closed before any request because their client held its share of them (--max-connections-per-client): it saw a reset, not an answer.",
                crate::listener::CLOSED_PAST_SHARE.load(std::sync::atomic::Ordering::Relaxed) as f64,
            ),
        ];
        let lags = self.follow_lags();
        if !lags.is_empty() {
            let family = |name, help, f: fn(&(String, f64, f64)) -> f64| Family { name, ty: "gauge", help, values: lags.iter().map(|l| (format!("name=\"{}\"", metrics::esc(&l.0)), f(l))).collect() };
            more.extend([
                family("chloedb_follow_lag_seconds", "How far a follow process writing through this server is behind its source, by the source's name: since the source first reached a transaction it has not applied (and while it is silent, since it last said).", |l| l.1),
                family("chloedb_follow_behind_transactions", "Transactions of the source a follow process writing through this server has not applied, over the source's shards.", |l| l.2),
            ]);
        }
        if let Some(s) = &self.log_store {
            more.extend(s.metrics());
        }
        // Models behind an API: what each field's model asked of it.
        let hosted: Vec<(String, chloedb::embed::Usage)> = self
            .models()
            .iter()
            .filter_map(|m| m.model.as_ref().ok().and_then(|e| e.usage()).map(|u| (format!("table=\"{}\",field=\"{}\",model=\"{}\"", metrics::esc(&m.table), metrics::esc(&m.field), metrics::esc(&m.spec.model)), u)))
            .collect();
        if !hosted.is_empty() {
            let family = |name, help, f: fn(&chloedb::embed::Usage) -> u64| Family { name, ty: "counter", help, values: hosted.iter().map(|(l, u)| (l.clone(), f(u) as f64)).collect() };
            more.extend([
                family("chloedb_embed_requests_total", "Requests sent to a hosted embedding model, by field.", |u| u.requests),
                family("chloedb_embed_retries_total", "Requests to a hosted embedding model sent again after a rate limit or a failure.", |u| u.retries),
                family("chloedb_embed_failures_total", "Requests to a hosted embedding model that failed for good, failing their write or query.", |u| u.failures),
                family("chloedb_embed_tokens_total", "Tokens a hosted embedding model counted, where its answers say.", |u| u.tokens),
            ]);
        }
        // Language models' answers to queries' llm() calls.
        let u = chloedb::query::llm::usage();
        more.extend([
            one("chloedb_llm_requests_total", "counter", "Requests this server made to language models for queries' llm() calls.", u.requests as f64),
            one("chloedb_llm_cached_total", "counter", "Answers to llm() calls this server found in memory, with no request.", u.cached as f64),
            one("chloedb_llm_failures_total", "counter", "Requests to language models for llm() calls that failed for good, failing their query.", u.failures as f64),
        ]);
        if let Some(b) = &self.query_memory {
            more.extend([
                one("chloedb_query_memory_bytes", "gauge", "Memory the running queries hold, as counted against their budget.", b.used() as f64),
                one("chloedb_query_memory_limit_bytes", "gauge", "Memory the running queries may hold together (--query-memory-mb).", b.limit() as f64),
                one("chloedb_query_memory_trips_total", "counter", "Queries refused because the running queries' memory budget was spent.", b.trips() as f64),
            ]);
        }
        match &self.backend {
            Backend::Local(db) => {
                let s = db.snapshot();
                let f = db.storage().file_stats();
                let m = db.storage().merge_status();
                let w = &self.writer_state;
                let deleted: u64 = f.segments.iter().map(|s| s.n_deleted as u64).sum::<u64>() + f.pending_deletes as u64;
                let pool = chloedb::query::pool::Pool::global();
                more.extend([
                    one("chloedb_query_pool_threads", "gauge", "Threads in the pool queries borrow from to search segments in parallel.", pool.threads() as f64),
                    one("chloedb_query_pool_queued", "gauge", "Threads queries asked the pool for that none was free to give yet.", pool.queued() as f64),
                    one("chloedb_docs", "gauge", "Rows in the database.", s.n_live_docs() as f64),
                    one("chloedb_segments", "gauge", "Segments on disk.", s.segments().len() as f64),
                    one("chloedb_deleted_docs", "gauge", "Deleted rows that merges have not dropped yet.", deleted as f64),
                    one("chloedb_memtable_docs", "gauge", "Rows written but not yet flushed to a segment.", f.memtable_docs as f64),
                    one("chloedb_file_bytes", "gauge", "Bytes the database takes on disk, without its write-ahead log.", f.file_bytes as f64),
                    one("chloedb_free_bytes", "gauge", "Bytes of the database file free for reuse (0 in the directory layout).", (f.free_pages * chloedb::storage::pager::PAGE) as f64),
                    one("chloedb_wal_bytes", "gauge", "Bytes of the write-ahead log.", f.wal_bytes as f64),
                    one("chloedb_snapshot_leases", "gauge", "Snapshot leases held.", self.sweep_leases() as f64),
                    one("chloedb_scrolls_open", "gauge", "Scrolls open: queries read a page at a time (page_size).", self.sweep_scrolls().0 as f64),
                    one("chloedb_scroll_rows_held", "gauge", "Rows, or rows' places, the open scrolls hold between pages (at most --max-scroll-rows).", self.sweep_scrolls().1 as f64),
                    one("chloedb_stale_stats_total", "counter", "Queries a coordinator sent with kept BM25 statistics of an older version, answered without running.", self.stale_stats.load(Ordering::Relaxed) as f64),
                    one("chloedb_failing", "gauge", "1 when the database cannot write until the server restarts.", self.failing().is_some() as u8 as f64),
                    one("chloedb_flush_failing", "gauge", "1 while flushes fail (a full disk).", db.storage().flush_error().is_some() as u8 as f64),
                    one("chloedb_write_queue_requests", "gauge", "Write requests in the write path: embedding, waiting for the writer, or being applied.", w.queued.load(Ordering::Relaxed) as f64),
                    one("chloedb_write_batches_total", "counter", "Groups of write requests the writer applied in one transaction.", w.batches.load(Ordering::Relaxed) as f64),
                    one("chloedb_writer_panics_total", "counter", "Groups of writes whose application panicked.", w.panics.load(Ordering::Relaxed) as f64),
                    one("chloedb_merges_running", "gauge", "Merges running now.", m.running as f64),
                    one("chloedb_merge_memory_bytes", "gauge", "Memory the running merges are estimated to use.", m.memory as f64),
                    one("chloedb_merges_total", "counter", "Merges finished.", m.merges as f64),
                    one("chloedb_merges_limited_total", "counter", "Merges cut, or put off, to fit the memory budget or an interrupted merge's.", m.merges_limited as f64),
                    one("chloedb_merges_interrupted", "gauge", "Groups of segments whose merge a crash interrupted.", m.interrupted as f64),
                    one("chloedb_merge_failing", "gauge", "1 while merges fail.", m.error.is_some() as u8 as f64),
                    one("chloedb_damaged_segments", "gauge", "Segments the last scrub found failing their checksums (--scrub-every-s).", self.scrubs.damaged().len() as f64),
                ]);
                if let Some(c) = db.storage().cold_stats() {
                    more.extend([
                        one("chloedb_cold_segments", "gauge", "Segments kept on object storage.", c.segments as f64),
                        one("chloedb_cold_bytes", "gauge", "Bytes of the segments kept on object storage.", c.bytes as f64),
                        one("chloedb_cold_cache_bytes", "gauge", "Bytes of cold segments in the local cache, their headers and keys included.", c.cached_bytes as f64),
                        one("chloedb_cold_cache_limit_bytes", "gauge", "The local cache's limit, headers and keys apart.", c.cache_limit as f64),
                        one("chloedb_cold_fetches_total", "counter", "Reads of object storage for cold segments' parts.", c.fetches as f64),
                        one("chloedb_cold_fetched_bytes_total", "counter", "Bytes read from object storage.", c.fetched_bytes as f64),
                        one("chloedb_cold_evictions_total", "counter", "Cold segments whose cached parts were dropped.", c.evictions as f64),
                        one("chloedb_cold_offloaded_total", "counter", "Segments made cold.", c.segments_offloaded as f64),
                    ]);
                }
                match &self.replication() {
                    Replication::None => {}
                    Replication::Primary(p) => more.extend(p.metrics()),
                    Replication::Replica(f) => more.extend(f.metrics()),
                }
            }
            Backend::Cluster(c) => more.extend(c.metrics()),
        }
        if let Some(a) = &self.answers {
            more.extend(a.metrics());
        }
        self.metrics.render(&more)
    }
}

pub enum SqlAnswer {
    Rows(Answer),
    Written { w: Written, took_ms: f64 },
}

/// The term of the primary that took a write ([`Service::admit_write`]),
/// which answers it once its copies hold it ([`Service::replicated`]):
/// `None` on a server of its own. And the write counted as taken until it
/// is applied or refused ([`crate::replication::Flight`]).
struct Admitted(Option<u64>, Option<crate::replication::Flight>);

/// What a write did: the rows it wrote and, in a database that numbers
/// its writes (`--seq-no`), its transaction's sequence number. Through a
/// coordinator, whose shards number their own writes, each shard's
/// instead (`seq_nos`, 0 for a shard not written or whose replicas each
/// number their own); empty when none is numbered. A read that passes
/// them on sees the write ([`After`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub rows: u64,
    pub seq_no: Option<u64>,
    pub seq_nos: Vec<u64>,
    /// Operations not applied because their source offset was written
    /// already ([`op_source`]): a replay's.
    pub skipped: u64,
    /// When the events the write applied happened (their `source.ts`, in
    /// microseconds), for the lag metric; not part of the answer.
    pub stamps: Vec<i64>,
    /// What the write's operations found ([`Receipt`]): `None` for a SQL
    /// statement, an answer replayed by its idempotency key, or through a
    /// coordinator a shard of a protocol before [`crate::grpc::RECEIPT_PROTOCOL`].
    pub receipt: Option<Receipt>,
    /// Through a coordinator: each shard written, its rows and receipt.
    pub shards: Vec<ShardWritten>,
    /// The operations the request carried (its marks of stream sources
    /// left out): 0 for a SQL statement.
    pub received: u64,
}

/// What one request's write operations found, counted by the server that
/// applied them ([`apply_into`]): rows inserted (no row had the key),
/// replaced (a document written over a row that was there), updated (a
/// `set` or an `add` of a row that was there), deleted, collapsed (a
/// document written over one an earlier operation of the same request
/// wrote: a key twice in one batch, which leaves one row), and not found
/// (a `set`, `add` or `delete` of a key no row has).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Receipt {
    pub inserted: u64,
    pub replaced: u64,
    pub updated: u64,
    pub deleted: u64,
    pub collapsed: u64,
    pub not_found: u64,
}

impl Receipt {
    pub fn add(&mut self, o: &Receipt) {
        self.inserted += o.inserted;
        self.replaced += o.replaced;
        self.updated += o.updated;
        self.deleted += o.deleted;
        self.collapsed += o.collapsed;
        self.not_found += o.not_found;
    }

    /// What `strict` refuses: a row replaced, a key twice, a key not found.
    pub fn mismatches(&self) -> Vec<String> {
        let mut m = Vec::new();
        for (n, what) in [(self.replaced, "replaced a row that was there"), (self.collapsed, "wrote a key an earlier operation of the request wrote"), (self.not_found, "found no row with its key")] {
            if n > 0 {
                m.push(format!("{n} operation{} {what}", if n == 1 { "" } else { "s" }));
            }
        }
        m
    }
}

impl Written {
    /// Adds `w`, another request's answer of the same write (an import's),
    /// to this one: its rows, receipt and shards' parts. Their sequence
    /// numbers are the last's.
    pub fn add(&mut self, w: Written) {
        self.rows += w.rows;
        self.skipped += w.skipped;
        self.received += w.received;
        self.receipt = match (self.receipt, w.receipt) {
            (Some(mut a), Some(b)) => {
                a.add(&b);
                Some(a)
            }
            _ => None,
        };
        for p in w.shards {
            match self.shards.iter_mut().find(|q| q.shard == p.shard) {
                Some(q) => {
                    q.rows += p.rows;
                    q.receipt = match (q.receipt, p.receipt) {
                        (Some(mut a), Some(b)) => {
                            a.add(&b);
                            Some(a)
                        }
                        _ => None,
                    };
                }
                None => self.shards.push(p),
            }
        }
        self.shards.sort_by_key(|p| p.shard);
        if w.seq_no.is_some() {
            self.seq_no = w.seq_no;
        }
        if !w.seq_nos.is_empty() {
            self.seq_nos = w.seq_nos;
        }
    }
}

/// How an import reads and writes its rows ([`Service::import`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct ImportOpts {
    /// Refuse a key twice in the file or one a row has already, and a CSV
    /// or TSV row with more or fewer values than its header, writing
    /// nothing.
    pub strict: bool,
    /// What an empty CSV or TSV cell means.
    pub empty: chloedb::ingest::EmptyCell,
}

/// One shard's part of a write through a coordinator ([`Written::shards`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShardWritten {
    pub shard: usize,
    pub rows: u64,
    pub receipt: Option<Receipt>,
}

/// Rows one write request of an import carries.
pub const IMPORT_OPS: usize = 1000;

/// Operations a batch of a stream (`/v1/stream`) takes at most; fewer when
/// the client has sent no more yet.
pub const STREAM_OPS: usize = 4096;

/// How often a stream, or a write held back by a backlog, looks again.
const STREAM_POLL: Duration = Duration::from_millis(5);

/// How long a primary given other replicas waits for the change before to
/// reach a majority of its copies ([`Service::configure_as`]): a heartbeat
/// or two, as a replica takes the membership in the next call.
pub const RECONFIGURE_WAIT: Duration = Duration::from_secs(3);

/// How long a write waits for a backlog of flushes to clear before 429.
const BACKLOG_WAIT: Duration = Duration::from_secs(1);

/// When a client should send a write refused for the server's load (429)
/// again, as `Retry-After` says.
pub const RETRY_AFTER: Duration = Duration::from_secs(1);

/// The largest body a route other than `/v1/write` and `/v1/import` takes:
/// a query, its parameters (vectors among them) or a synonym list.
pub const SMALL_BODY: usize = 16 << 20;

/// The shard of `key` among `n` under an even slot map
/// ([`slots::shard_of`]), as a coordinator without a map of its own routes
/// writes. Loaders must split documents the same way.
pub fn shard_of(key: &Value, n: usize) -> usize {
    slots::shard_of(key, n.max(1))
}

/// What an idempotency key of a write statement names: its text, its
/// parameters (by name) and the slots it is limited to.
fn statement_hash(sql: &str, params: &Params, slots: Option<&[usize]>) -> u64 {
    let mut named: Vec<(&String, String)> = params.0.iter().map(|(k, v)| (k, format!("{v:?}"))).collect();
    named.sort();
    idem::hash_ops(&[json!(sql), json!(named), json!(slots)])
}

fn conflict(message: String) -> ApiError {
    ApiError { code: None, status: 409, message }
}

/// What a shard says of a request routed by a newer cluster map than the
/// one it holds (503): its coordinator gives it the map and asks again.
pub const OLDER_THAN_REQUEST: &str = "older than the request's";

/// What a shard without a cluster map says of a request routed by one
/// (503): it is being given one, or its coordinator gives it one.
pub const NO_MAP_YET: &str = "this shard has no slot map";

/// In the message of a request a shard refuses (409) as routed by a map
/// older than its own: one the cluster changed since.
pub const OLDER_THAN_SHARD: &str = "is older than this shard's";

/// Why a write refuses `min_seq_no` and `min_seq_nos`.
const FOR_READS: &str = "min_seq_no and min_seq_nos are for reads: a write sees every write before it";

/// A query refused for want of a slot ([`Service::admit`]).
/// A shard's answer to a coordinator's leaf ([`QueryReq::arrow`]): its
/// rows, or its statistics and its rows as an Arrow IPC stream.
fn shard_leaf(leaf: Leaf) -> chloedb::Result<(ResultSet, Vec<u8>)> {
    match leaf {
        Leaf::Rows(rs) => Ok((rs, Vec::new())),
        #[cfg(feature = "datafusion")]
        Leaf::Arrow(c) => {
            let ipc = c.to_ipc()?;
            let rs = ResultSet { columns: c.names, rows: Vec::new(), keys: Vec::new(), scores: Vec::new(), parts: None, stats: c.stats };
            Ok((rs, ipc))
        }
    }
}

fn too_many() -> ApiError {
    ApiError { code: None, status: 429, message: "too many queries running; try again".into() }
}

/// How many rows of `db`, of any table, are placed outside shard
/// `shard`'s slots under `map`, by their key or their table's shard_by
/// value ([`Schema::with_shard_by`]), and one of those values; `None` if
/// every row is in them. Reads every row's.
fn rows_elsewhere(db: &Db, map: &SlotMap, shard: usize) -> Result<Option<(usize, Value)>, ApiError> {
    let snap = db.snapshot();
    let opts = ExecOptions { max_rows: None, ..db.exec.clone() };
    let mut out: Option<(usize, Value)> = None;
    for schema in db.catalog() {
        let Some(place) = schema.shard_by_field().or(schema.primary_key) else { continue };
        let view = snap.table(&schema.table).ok_or_else(|| ApiError::internal(format!("no table {} in its own snapshot", schema.table)))?;
        let sql = format!("SELECT {} FROM {}", quoted(&schema.fields[place].name), ident(&schema.table));
        let select = bind(&sql, &schema, &Params::new())?;
        let rs = db.execute(&view, &select, &opts)?;
        let values: Vec<Value> = match schema.shard_by_field() {
            Some(_) => rs.rows.into_iter().filter_map(|r| r.into_iter().next()).collect(),
            None => rs.keys,
        };
        // Rows of slots moving that the shard holds without serving are
        // in place too.
        let residue = map.residue(shard).unwrap_or_default();
        for v in values.into_iter().filter(|v| map.shard_of(v) != shard && residue.binary_search(&slots::slot(v)).is_err()) {
            match &mut out {
                Some((n, _)) => *n += 1,
                None => out = Some((1, v)),
            }
        }
    }
    Ok(out)
}

/// Writes `map`, `shard` and the epoch since which the shard holds its
/// slots to `path` ([`save_json`]).
fn save_slots(path: &Path, map: &SlotMap, shard: usize, since: u64) -> std::io::Result<()> {
    save_json(path, &json!({"shard": shard, "since": since, "map": map.to_json()}))
}

/// Writes `j` to `path` and syncs it, through a new file renamed over the
/// old one, so that a crash leaves one or the other. One at a time: two at
/// once would write the same new file (a primary's vote kept as a call of
/// the new primary fences it), and leave it torn or gone.
pub(crate) fn save_json(path: &Path, j: &Json) -> std::io::Result<()> {
    use std::io::Write as _;
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let body = serde_json::to_vec_pretty(j).map_err(std::io::Error::other)?;
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    match path.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(dir) => std::fs::File::open(dir)?.sync_all(),
        None => Ok(()),
    }
}

/// The table an operation writes: its `"table"`, else `first`.
fn op_table<'a>(op: &'a Json, first: &'a str) -> Result<&'a str, ApiError> {
    match op.get("table") {
        None | Some(Json::Null) => Ok(first),
        Some(Json::String(t)) => Ok(t),
        Some(_) => Err(ApiError::bad("an operation's \"table\" is a table's name")),
    }
}

/// The table of `catalog` (the database's tables, the first first) that an
/// operation writes ([`op_table`]).
pub fn op_schema<'a>(catalog: &'a [Arc<Schema>], op: &Json) -> Result<&'a Arc<Schema>, ApiError> {
    let first = op_default(catalog)?;
    let name = op_table(op, &first.table)?;
    let t = catalog.iter().find(|t| t.table.eq_ignore_ascii_case(name)).ok_or_else(|| ApiError::bad(format!("query error: no such table: {name}")))?;
    if t.is_placeholder() {
        return Err(ApiError::bad(NO_TABLES));
    }
    Ok(t)
}

/// The table of `catalog` an operation that names none writes: the first,
/// or on a database made with no tables the first made after its
/// placeholder ([`Service::default_table`]).
fn op_default(catalog: &[Arc<Schema>]) -> Result<&Arc<Schema>, ApiError> {
    let first = catalog.first().ok_or_else(|| ApiError::internal("a database without tables"))?;
    if !first.is_placeholder() {
        return Ok(first);
    }
    Ok(catalog.iter().find(|t| !t.is_placeholder()).unwrap_or(first))
}

/// What a request that names no table is told on a database made with no
/// tables, before its first ([`Service::default_table`]).
pub const NO_TABLES: &str = "this database has no tables yet: create one (CREATE TABLE, POST /v1/tables, or an import that creates it), then name it";

/// What places the row an operation writes on a shard: its key, or for a
/// table placed by a column ([`Schema::with_shard_by`]) that column's
/// value, from the operation's document or, for an operation by key, its
/// `"routing"`.
pub fn op_place(schema: &Schema, op: &Json) -> Result<Value, ApiError> {
    let Some(f) = schema.shard_by_field() else { return op_key(schema, op) };
    let name = schema.fields[f].name.as_str();
    let given = op.get("doc").and_then(|d| d.get(name)).filter(|v| !v.is_null()).or_else(|| op.get("routing").filter(|v| !v.is_null()));
    match given {
        Some(v) => js::field_value(schema, f, v),
        None => Err(ApiError::bad(format!("{} places its rows on shards by {name} (shard_by): the operation needs the row's {name}, in its doc or as \"routing\"", schema.table))),
    }
}

/// What places a row of `schema`'s table on a shard, as messages name
/// it: `key`, or the shard_by field.
fn place_name(schema: &Schema) -> &str {
    schema.shard_by.as_deref().unwrap_or("key")
}

/// The primary key an operation names (`key`, or the document's).
pub fn op_key(schema: &Schema, op: &Json) -> Result<Value, ApiError> {
    let pk = schema.primary_key.ok_or_else(|| ApiError::bad("writes by key need a primary key"))?;
    match (op.get("key"), op.get("doc")) {
        (Some(k), _) => js::field_value(schema, pk, k),
        (None, Some(d)) => js::field_value(schema, pk, d.get(&schema.fields[pk].name).unwrap_or(&Json::Null)),
        _ => Err(ApiError::bad("an operation needs a key or a doc")),
    }
}

/// A write request for the writer thread.
enum WriteJob {
    /// Operations of `/v1/write` (or gRPC `Write`).
    Ops(Vec<Json>),
    /// An `INSERT`, `UPDATE` or `DELETE`, its vectors computed.
    Sql(Box<PreparedWrite>),
}

struct Queued {
    job: WriteJob,
    deadline: Instant,
    /// `None` once answered.
    reply: Option<tokio::sync::oneshot::Sender<Result<Written, ApiError>>>,
    /// The request's idempotency key and the hash of what it names, whose
    /// outcome the writer records ([`idem`]).
    key: Option<(String, u64)>,
    keys: Arc<idem::Keys>,
    /// Counts the request as queued until it is answered (`None` then).
    queued: Option<InQueue>,
    /// Held until the write commits, for a hold on writes to moving slots
    /// to wait for ([`Service::pass_gate`]).
    pass: Option<moving::Pass>,
    /// The offset its transaction writes in `_sources` to mark it as a
    /// slot move's ([`feed::MOVE_COPY`], [`feed::MOVE_CLEANUP`]): such a
    /// request shares its transaction only with others of the same mark,
    /// so that a shard's feed leaves out the move's rows and only them.
    mark: Option<(&'static str, i64)>,
    /// The epoch of the cluster map the request was routed by (0: none),
    /// which says which slots its stream events cover ([`arrivals`]).
    epoch: u64,
    /// The term it was taken in by a primary (`None`: a server that does
    /// not replicate, or a slot move's own write): it commits only while
    /// the database is at that term ([`commits_in`]).
    term: Option<u64>,
}

impl Queued {
    /// Records the outcome under the request's key and answers it.
    fn answer(mut self, r: Result<Written, ApiError>) {
        if let Some((k, _)) = &self.key {
            self.keys.finish(k, r.as_ref().ok().cloned());
        }
        // Uncounted before the answer, which its caller may follow with a
        // look at the metrics.
        self.queued = None;
        self.pass = None;
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(r);
        }
    }
}

impl Drop for Queued {
    /// A request dropped unanswered: the writer panicked applying it.
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            if let Some((k, _)) = &self.key {
                self.keys.finish(k, None);
            }
            let _ = reply.send(Err(ApiError::internal("the writer failed applying this write (see the server's log)")));
        }
    }
}

/// The writer thread's state, for health checks and metrics.
pub struct WriterState {
    /// False once the thread has stopped.
    pub alive: AtomicBool,
    /// Groups of write requests whose application panicked.
    pub panics: AtomicU64,
    /// Groups of write requests applied, each in one transaction (or
    /// each request alone, after the group's failed).
    pub batches: AtomicU64,
    /// Write requests queued for the writer or being applied.
    pub queued: Arc<AtomicUsize>,
}

impl Default for WriterState {
    fn default() -> WriterState {
        WriterState { alive: AtomicBool::new(true), panics: AtomicU64::new(0), batches: AtomicU64::new(0), queued: Arc::default() }
    }
}

/// Counts a write request as queued until dropped.
struct InQueue(Arc<AtomicUsize>);

impl InQueue {
    /// Counts a request as queued, unless `max` already are.
    fn start(state: &WriterState, max: usize) -> Option<InQueue> {
        let before = state.queued.fetch_add(1, Ordering::Relaxed);
        // Dropped at once when refused, which uncounts it.
        let q = InQueue(state.queued.clone());
        (before < max).then_some(q)
    }
}

impl Drop for InQueue {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Marks the writer stopped when its thread ends, however it ends.
struct Alive<'a>(&'a WriterState);

impl Drop for Alive<'_> {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Relaxed);
    }
}

/// Counts a query among those running until dropped. The blocking work
/// holds it, so a query whose caller stopped waiting still counts while
/// it runs.
struct Running(Arc<AtomicUsize>);

impl Running {
    /// Starts counting; returns the guard and how many run now.
    fn start(n: &Arc<AtomicUsize>) -> (Running, usize) {
        let now = n.fetch_add(1, Ordering::Relaxed) + 1;
        (Running(n.clone()), now)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A query's cancellation flag ([`ExecOptions::cancel`]), set when this
/// is dropped: when the query's future ends, finished or dropped by its
/// caller (a client that went away, a hedge that lost, a shard timeout).
#[derive(Default)]
struct Cancel(Arc<AtomicBool>);

impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The longest statement a request plans on the thread that serves it:
/// planning one this short takes microseconds. A longer one plans on a
/// blocking thread ([`Service::plan_read`]).
const PLAN_INLINE_BYTES: usize = 2048;

/// Operations one shared transaction takes at most (an SQL write counts
/// its rows or keys, [`PreparedWrite::weight`]).
const GROUP_OPS: usize = chloedb::db::GROUP_OPS;

/// The writer of a local database: one thread applying write requests in
/// the order they arrive. Requests that wait together share one
/// transaction, so one commit and one disk flush serve them all (group
/// commit); if one of them fails, each runs again alone, so that only it
/// fails. Operations (`/v1/write`) group with operations. SQL writes find
/// their rows by reading the database, which does not show a
/// transaction's own changes, so they group with SQL writes that touch
/// other rows ([`Footprints`]): an INSERT, or an UPDATE or DELETE by key,
/// of keys no other in the group has. A group that panics is answered
/// with errors and the writer goes on.
fn writer(db: &Db, rx: mpsc::Receiver<Queued>, state: &WriterState) {
    let _alive = Alive(state);
    let mut next = None;
    loop {
        let Some(first) = next.take().or_else(|| rx.recv().ok()) else { return };
        let mut n = first.weight();
        let mut prints = Footprints::default();
        if let WriteJob::Sql(p) = &first.job {
            prints.admit(p);
        }
        let ops = matches!(first.job, WriteJob::Ops(_));
        let mark = first.mark;
        let mut group = vec![first];
        while n < GROUP_OPS {
            match rx.try_recv() {
                Ok(q) if ops && matches!(q.job, WriteJob::Ops(_)) && q.mark == mark => {
                    n += q.weight();
                    group.push(q);
                }
                Ok(q) if matches!(&q.job, WriteJob::Sql(p) if !ops && n + p.weight() <= GROUP_OPS && prints.admit(p)) => {
                    n += q.weight();
                    group.push(q);
                }
                Ok(q) => {
                    next = Some(q);
                    break;
                }
                Err(_) => break,
            }
        }
        // The requests a panic leaves unanswered are answered as they drop.
        state.batches.fetch_add(1, Ordering::Relaxed);
        let n = group.len();
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply_group(db, group))).is_err() {
            state.panics.fetch_add(1, Ordering::Relaxed);
            tracing::error!(requests = n, "the writer panicked applying a group of writes; they were answered with errors");
        }
    }
}

/// Applies one group of write requests and answers each.
fn apply_group(db: &Db, group: Vec<Queued>) {
    // A request still queued at its deadline is refused unapplied.
    let now = Instant::now();
    let (late, group): (Vec<Queued>, Vec<Queued>) = group.into_iter().partition(|q| q.deadline <= now);
    for q in late {
        q.answer(Err(ApiError { code: None, status: 503, message: "the deadline passed while waiting for the writer".into() }));
    }
    // Taken by a primary of a term the database left (it stepped down, and
    // took its new primary's transactions): refused unapplied.
    let (gone, group): (Vec<Queued>, Vec<Queued>) = group.into_iter().partition(|q| commits_in(db, q.term).is_err());
    for q in gone {
        let e = commits_in(db, q.term).unwrap_err();
        q.answer(Err(e));
    }
    let Some(deadline) = group.iter().map(|q| q.deadline).max() else { return };
    let sql: Vec<&PreparedWrite> = group.iter().filter_map(|q| q.sql()).collect();
    if !sql.is_empty() {
        let keys: Vec<Option<(&str, u64)>> = group.iter().map(Queued::key).collect();
        // Each key in its write's transaction, if it wrote a row ([`idem::record`]).
        let run = |ps: &[&PreparedWrite], keys: &[Option<(&str, u64)>], deadline: Instant| {
            let now = chloedb::timestamp::now();
            let record = |t: &mut WriteTxn<'_>, rows: &[u64]| keys.iter().zip(rows).filter(|(_, &rows)| rows > 0).try_for_each(|(k, &rows)| k.map_or(Ok(()), |(k, hash)| idem::record(t, k, hash, rows, now)));
            wait_for_writer(deadline, || {
                // Made outside the transaction, which holds the writer.
                if keys.iter().any(Option::is_some) {
                    db.storage().ensure_sources()?;
                }
                db.write_prepared_group_then(ps, record).map_err(ApiError::from)
            })
        };
        let answers: Vec<Result<Written, ApiError>> = match run(&sql, &keys, deadline) {
            Ok((rows, seq_no)) => rows.into_iter().map(|rows| Ok(Written { rows, seq_no, ..Default::default() })).collect(),
            Err(e) if sql.len() == 1 => vec![Err(e)],
            // A failed commit wrote nothing: each runs again alone.
            Err(_) => sql.iter().zip(&keys).zip(&group).map(|((p, k), q)| run(&[*p], &[*k], q.deadline).map(|(rows, seq_no)| Written { rows: rows[0], seq_no, ..Default::default() })).collect(),
        };
        drop(keys);
        drop(sql);
        for (q, r) in group.into_iter().zip(answers) {
            q.answer(r);
        }
        return;
    }
    let requests: Vec<(&[Json], u64)> = group.iter().map(|q| (q.ops(), q.epoch)).collect();
    let keys: Vec<Option<(&str, u64)>> = group.iter().map(Queued::key).collect();
    // A group shares one mark ([`writer`]), and a term, checked again under
    // the writer: requests of another term fail it, and run alone.
    let mark = group[0].mark;
    let term = group[0].term;
    let answers = wait_for_writer(deadline, || apply_keyed_in(db, &requests, &keys, mark, term));
    drop((requests, keys));
    match answers {
        Ok((applied, seq_no)) => {
            for (q, a) in group.into_iter().zip(applied) {
                q.answer(Ok(a.written(seq_no)));
            }
        }
        Err(e) if group.len() == 1 => {
            group.into_iter().next().expect("one").answer(Err(e));
        }
        // A failed commit wrote nothing (Strata reports a durable commit
        // as Ok), so each request can run again alone.
        Err(_) => {
            for q in group {
                let r = wait_for_writer(q.deadline, || apply_keyed_in(db, &[(q.ops(), q.epoch)], &[q.key()], q.mark, q.term).map(|(mut a, seq_no)| a.remove(0).written(seq_no)));
                q.answer(r);
            }
        }
    }
}

impl Queued {
    fn ops(&self) -> &[Json] {
        match &self.job {
            WriteJob::Ops(ops) => ops,
            WriteJob::Sql(..) => &[],
        }
    }

    fn sql(&self) -> Option<&PreparedWrite> {
        match &self.job {
            WriteJob::Ops(..) => None,
            WriteJob::Sql(p) => Some(p),
        }
    }

    /// Its idempotency key and the hash of what it names.
    fn key(&self) -> Option<(&str, u64)> {
        self.key.as_ref().map(|(k, h)| (k.as_str(), *h))
    }

    /// How much of a group it takes ([`GROUP_OPS`]).
    fn weight(&self) -> usize {
        match &self.job {
            WriteJob::Ops(ops) => ops.len(),
            WriteJob::Sql(p) => p.weight(),
        }
    }
}

/// A checkpoint's name in the backup directory: letters, digits, '.', '_'
/// and '-', not starting with '.', so that it stays in the directory.
fn checkpoint_name(name: &str) -> Result<&str, ApiError> {
    let ok = !name.is_empty() && name.len() <= 200 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    ok.then_some(name).ok_or_else(|| ApiError::bad(format!("bad checkpoint name {name:?}: use letters, digits, '.', '_' and '-', not starting with '.'")))
}

/// Runs `write` until it gets the writer or `deadline` passes: Strata
/// answers `Busy` (503) rather than queueing while another write, or a
/// background merge's commit, holds it. A busy write has changed nothing.
pub(crate) fn wait_for_writer<T>(deadline: Instant, mut write: impl FnMut() -> Result<T, ApiError>) -> Result<T, ApiError> {
    loop {
        match write() {
            Err(e) if e.status == 503 && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            r => return r,
        }
    }
}

/// The transaction a shard writes as it takes cluster map `new` in place
/// of `old`, before the map lets a write reach the slots that change hands
/// here: their marks for a cluster following this one ([`feed::flip_marks`],
/// each an offset `new.epoch` that `_sources` does not keep), and the stream
/// offsets of the slots that arrive ([`arrivals::flip`]). Nothing when
/// neither has anything to write.
fn write_flip(db: &Db, marks: &[String], old: &SlotMap, new: &SlotMap, shard: usize) -> Result<(), ApiError> {
    let offsets = new.moving().is_some_and(|m| m.to == shard && !m.offsets.is_empty());
    if marks.is_empty() && !offsets && db.sources()?.is_empty() {
        return Ok(());
    }
    db.storage().ensure_sources()?;
    let mut t = db.storage().begin()?;
    let now = chloedb::timestamp::now();
    for m in marks {
        t.mark_source(m, new.epoch as i64, now)?;
    }
    let kept = arrivals::flip(db, &mut t, old, new, shard, now)?;
    if marks.is_empty() && !kept {
        t.rollback();
        return Ok(());
    }
    t.commit_seq()?;
    Ok(())
}

/// Applies `ops` to `db` in one transaction.
pub fn apply_ops(db: &Db, ops: &[Json]) -> Result<Written, ApiError> {
    let (mut applied, seq_no) = apply_requests(db, &[(ops, 0)], None)?;
    Ok(applied.remove(0).written(seq_no))
}

/// What one request's operations did in a transaction ([`apply_into`]).
#[derive(Default)]
struct Applied {
    rows: u64,
    skipped: u64,
    stamps: Vec<i64>,
    receipt: Receipt,
}

impl Applied {
    fn written(self, seq_no: Option<u64>) -> Written {
        Written { rows: self.rows, seq_no, skipped: self.skipped, stamps: self.stamps, receipt: Some(self.receipt), ..Default::default() }
    }
}

/// Applies several requests' operations, each with the epoch of the
/// cluster map it was routed by, in one transaction; returns what each
/// did, and the transaction's sequence number in a database that numbers
/// its writes. With `mark`, the transaction also writes that offset of a
/// source of the server's own ([`feed::MARK`]).
fn apply_requests(db: &Db, requests: &[(&[Json], u64)], mark: Option<(&str, i64)>) -> Result<(Vec<Applied>, Option<u64>), ApiError> {
    apply_keyed(db, requests, &[], mark)
}

/// [`apply_requests`], recording in the transaction the idempotency key
/// (and the hash of what it names) of each request that has one, by the
/// request's place in `keys` ([`idem::record`]), if it wrote a row: one
/// that wrote none leaves no transaction behind it, as before.
fn apply_keyed(db: &Db, requests: &[(&[Json], u64)], keys: &[Option<(&str, u64)>], mark: Option<(&str, i64)>) -> Result<(Vec<Applied>, Option<u64>), ApiError> {
    apply_keyed_in(db, requests, keys, mark, None)
}

/// Whether a write taken in `term` ([`Queued::term`]) may commit on `db`
/// now: while the database is at that term. A primary that stepped down
/// (it voted for another copy in a handover) and then applied its new
/// primary's first transaction would otherwise commit a write it took
/// before under the new term's numbering, at a number the new primary
/// gives another transaction of the same term: two copies would hold
/// different transactions under one (term, number), which shipping takes
/// for the same history and never repairs (found by
/// `replication::a_write_applied_as_its_primary_hands_over_is_answered_in_doubt`
/// once each term began with a transaction, finding DS-5). Refused, it is
/// answered in doubt (409), as a write a replaced primary applied is, and
/// a coordinator does not send it again.
fn commits_in(db: &Db, term: Option<u64>) -> Result<(), ApiError> {
    let Some(t) = term else { return Ok(()) };
    let now = db.storage().numbering().map_or(0, |n| n.term);
    if now == t {
        return Ok(());
    }
    Err(conflict(format!(
        "the write was not applied: this server stopped being the primary of term {t} (its database is at term {now}) before it was acknowledged, and the new primary may not hold it: write to the new primary"
    )))
}

/// [`apply_keyed`], committing only while the database is at `term`
/// ([`commits_in`]), checked under the writer.
fn apply_keyed_in(db: &Db, requests: &[(&[Json], u64)], keys: &[Option<(&str, u64)>], mark: Option<(&str, i64)>, term: Option<u64>) -> Result<(Vec<Applied>, Option<u64>), ApiError> {
    // Made outside the transaction, which holds the writer.
    if mark.is_some() || keys.iter().any(Option::is_some) || requests.iter().flat_map(|(ops, _)| ops.iter()).any(has_source) {
        db.storage().ensure_sources()?;
    }
    let mut t = db.storage().begin()?;
    // The tables as the writer sees them: an ALTER TABLE that committed
    // while this write waited for the writer has changed their fields,
    // and the documents are made of the fields they have now.
    let catalog = db.catalog();
    // Under the writer, which a primary's shipped transaction needs too.
    if let Err(e) = commits_in(db, term) {
        t.rollback();
        return Err(e);
    }
    let mut applied = Vec::with_capacity(requests.len());
    let mut arrived = arrivals::Group::new(db);
    let now = chloedb::timestamp::now();
    for (i, (ops, epoch)) in requests.iter().enumerate() {
        let a = apply_into(&catalog, &mut t, ops, *epoch, &mut arrived)?;
        if let Some(Some((k, hash))) = keys.get(i).filter(|_| a.rows > 0) {
            idem::record(&mut t, k, *hash, a.rows, now)?;
        }
        applied.push(a);
    }
    if let Some((name, offset)) = mark {
        t.mark_source(name, offset, chloedb::timestamp::now())?;
    }
    let seq_no = t.commit_seq()?;
    Ok((applied, seq_no))
}

/// [`Service::validate_ops`]: each of `ops` applied in turn in one
/// transaction, which is rolled back.
fn validate_into(db: &Db, ops: &[Json], epoch: u64) -> Result<(), ApiError> {
    let mut t = db.storage().begin_within(VALIDATE_WAIT)?;
    // Read under the writer, as [`apply_keyed_in`] reads them.
    let catalog = db.catalog();
    let mut arrived = arrivals::Group::new(db);
    for (j, op) in ops.iter().enumerate() {
        if let Err(e) = apply_into(&catalog, &mut t, std::slice::from_ref(op), epoch, &mut arrived) {
            t.rollback();
            // The writer was busy: asked again (`wait_for_writer`).
            if e.status == 503 {
                return Err(e);
            }
            return Err(ApiError { code: None, status: e.status, message: format!("{VALIDATE_OP} {j}{VALIDATE_OF}{}", e.message) });
        }
    }
    t.rollback();
    Ok(())
}

/// How long [`Service::validate_ops`] waits for a busy writer before it
/// passes the part unchecked.
const VALIDATE_WAIT: Duration = Duration::from_millis(100);

/// How [`validate_into`] names the operation it refused: "operation N of
/// this shard's part: ...", which a coordinator reads back
/// ([`crate::coord`]).
pub const VALIDATE_OP: &str = "operation";
pub const VALIDATE_OF: &str = " of this shard's part: ";

/// Refuses a request whose operations carry the mark of a cluster
/// followed ([`feed::FOLLOWS`]) unless that cluster's follow process sends
/// it ([`following`]), or another of the server's own marks
/// ([`feed::own_mark`]) unless the cluster's own does ([`own`]): checked
/// as the request comes, before the writer thread, which knows no
/// request's sender, applies it.
pub(crate) fn own_marks_ok(ops: &[Json]) -> Result<(), ApiError> {
    let sender = following();
    for op in ops {
        let Some(name) = op.pointer("/source/name").and_then(Json::as_str) else { continue };
        let theirs = match name.strip_prefix(feed::FOLLOWS) {
            Some(n) => sender.as_deref() == Some(n),
            None => !feed::own_mark(name) || own(),
        };
        if !theirs {
            return Err(ApiError::bad(format!("source {name}: names starting with {MARK} are the server's own")));
        }
    }
    Ok(())
}

/// Where an operation's event came from (its `"source"`): a stream's
/// source, the event's offset in it, and when the event happened
/// (microseconds since the epoch), if the operation says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub name: String,
    pub offset: i64,
    pub ts: Option<i64>,
}

/// The longest source name, in bytes.
const SOURCE_NAME: usize = 256;

pub fn has_source(op: &Json) -> bool {
    op.get("source").is_some_and(|s| !s.is_null())
}

/// The scope a SQL statement needs ([`crate::keys`]): a query read, a
/// write write, and a table's, rank profile's or stored function's
/// statement admin. Every
/// kind is named, so that a new one must say what it needs.
pub fn command_scope(c: &Command) -> Scope {
    match c {
        Command::Query | Command::Explain { .. } | Command::ShowTables => Scope::Read,
        Command::Write { .. } => Scope::Write,
        Command::CreateTable { .. } | Command::AlterTable { .. } | Command::DropTable { .. } | Command::CreateRankProfile { .. } | Command::DropRankProfile { .. } | Command::Function(_) => Scope::Admin,
    }
}

/// Whether an operation only marks a source's offset (`"op": "source"`),
/// writing no row.
pub fn is_mark(op: &Json) -> bool {
    op.get("op").and_then(|o| o.as_str()) == Some("source")
}

/// An operation's `"source": {"name": ..., "offset": n, "ts": ...}`, if it
/// has one; `ts` is ISO 8601 text, or milliseconds since the epoch.
pub fn op_source(op: &Json) -> Result<Option<Mark>, ApiError> {
    let s = match op.get("source") {
        None | Some(Json::Null) => return Ok(None),
        Some(s) => s,
    };
    let name = s
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty() && n.len() <= SOURCE_NAME)
        .ok_or_else(|| ApiError::bad(format!("an operation's source has a \"name\" of 1 to {SOURCE_NAME} bytes")))?;
    // But for the marks of the server's state, which a request may carry
    // only from the cluster's own ([`own_marks_ok`]).
    if name.starts_with(MARK) && !feed::own_mark(name) {
        return Err(ApiError::bad(format!("source {name}: names starting with {MARK} are the server's own")));
    }
    let offset = s.get("offset").and_then(|o| o.as_i64()).ok_or_else(|| ApiError::bad(format!("source {name}: the \"offset\" is a whole number")))?;
    let ts = match s.get("ts") {
        None | Some(Json::Null) => None,
        Some(Json::String(t)) => Some(chloedb::timestamp::parse(t).ok_or_else(|| ApiError::bad(format!("source {name}: ts {t:?} is not ISO 8601 text")))?),
        Some(v) => Some(v.as_i64().ok_or_else(|| ApiError::bad(format!("source {name}: ts is ISO 8601 text or whole milliseconds since the epoch")))?.saturating_mul(1000)),
    };
    Ok(Some(Mark { name: name.to_string(), offset, ts }))
}

/// A stream's state from one batch to the next ([`Service::stream_write`]).
pub struct Stream {
    /// Each source's highest offset so far, which the next must pass.
    last: HashMap<String, i64>,
    /// The sources admitted since [`Stream::offsets`] last took them.
    fresh: Vec<String>,
    /// Names the stream's batches in their idempotency keys.
    id: u64,
    pub batches: u64,
    pub written: u64,
    pub skipped: u64,
}

impl Default for Stream {
    fn default() -> Stream {
        Stream { last: HashMap::new(), fresh: Vec::new(), id: rand_id(), batches: 0, written: 0, skipped: 0 }
    }
}

impl Stream {
    /// Takes the operations of one event or of several (a line of
    /// `/v1/stream`, a message of gRPC `WriteStream`), which a batch never
    /// splits: each source's offsets must pass those before them in the
    /// stream, since an offset written already marks a replay and is
    /// skipped.
    pub fn admit(&mut self, ops: &[Json]) -> Result<(), ApiError> {
        let mut top: Vec<(String, i64)> = Vec::new();
        for op in ops {
            let Some(m) = op_source(op)? else {
                if is_mark(op) {
                    return Err(ApiError::bad("a \"source\" operation needs a source: {\"op\": \"source\", \"source\": {\"name\": ..., \"offset\": n}}"));
                }
                continue;
            };
            if let Some(&last) = self.last.get(&m.name) {
                if m.offset <= last {
                    return Err(ApiError::bad(format!(
                        "source {}: offset {} comes after {last} in this stream: offsets increase along a stream, and one event's operations go together (one line, as {{\"ops\": [...]}}, or one message)",
                        m.name, m.offset
                    )));
                }
            }
            match top.iter_mut().find(|(n, _)| *n == m.name) {
                Some((_, t)) => *t = (*t).max(m.offset),
                None => top.push((m.name, m.offset)),
            }
        }
        for (name, offset) in top {
            if !self.fresh.contains(&name) {
                self.fresh.push(name.clone());
            }
            self.last.insert(name, offset);
        }
        Ok(())
    }

    /// The last offset of each source admitted since the last call: once
    /// a batch has committed, where each of its sources stands, which a
    /// consumer may commit to its log.
    pub fn offsets(&mut self) -> Vec<(String, i64)> {
        let fresh = std::mem::take(&mut self.fresh);
        fresh.into_iter().map(|n| { let o = self.last[&n]; (n, o) }).collect()
    }
}

fn rand_id() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish()
}

/// Refuses an operation with `if_seq_no` unless the row with `key` is
/// live with that sequence number, as this transaction finds it (a row an
/// earlier operation of the transaction wrote has none yet). Checked
/// under the writer lock, so no other write comes between it and the
/// operation.
fn check_seq_no(t: &TableTxn<'_, '_>, op: &Json, key: &Value) -> Result<(), ApiError> {
    let want = match op.get("if_seq_no") {
        None | Some(Json::Null) => return Ok(()),
        Some(v) => v.as_u64().ok_or_else(|| ApiError::bad("if_seq_no is a sequence number: a whole number from 0"))?,
    };
    let now = t.seq_of(key).map_err(|e| match e {
        chloedb::Error::Schema(_) => ApiError::bad("if_seq_no needs a database that numbers its writes: start the server with --seq-no"),
        e => e.into(),
    })?;
    if now == Some(want) {
        return Ok(());
    }
    let found = match now {
        Some(n) => format!("its sequence number is {n}"),
        None => "no row has the key, or an earlier operation of this request wrote it".to_string(),
    };
    Err(ApiError { code: None, status: 409, message: format!("if_seq_no {want} does not hold for key {}: {found}", key_text(key)) })
}

fn key_text(key: &Value) -> String {
    chloedb::query::hint::sql_value(key)
}

/// What an operation of a request's write found ([`Receipt`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    Inserted,
    Replaced,
    Collapsed,
    Updated,
    Deleted,
}

impl Found {
    /// A document written over a key this request wrote (`again`), or one
    /// a row had (`existed`).
    fn of(again: bool, existed: bool) -> Found {
        match (again, existed) {
            (true, _) => Found::Collapsed,
            (false, true) => Found::Replaced,
            (false, false) => Found::Inserted,
        }
    }
}

/// A `set`, `add` or `delete` that found no row with its key, refused
/// (`"if_missing": "error"`, which a coordinator gives an operation with a
/// `routing`, or `strict`): 404, naming the routing a table placed by a
/// column was given.
fn not_found(schema: &Schema, op: &Json, kind: &str) -> ApiError {
    let key = op_key(schema, op).map(|k| key_text(&k)).unwrap_or_else(|_| "?".into());
    let message = match (op.get("routing").filter(|r| !r.is_null()), schema.shard_by.as_deref()) {
        (Some(r), Some(by)) => format!(
            "{kind} found no row of {} with key {key} where its routing {r} ({by}) places it: on a cluster the row is on the shard of its own {by}, so give that as \"routing\" (or \"if_missing\": \"skip\" to pass over a key that is not there); nothing was written",
            schema.table
        ),
        _ => format!("{kind} found no row of {} with key {key}; nothing was written (\"if_missing\": \"skip\" passes over a key that is not there)", schema.table),
    };
    ApiError { code: None, status: 404, message }
}

/// Applies one request's `ops`, each to the table it names ([`op_table`]);
/// a TIMESTAMP defaulting to `"now"` takes the same time in all of them.
///
/// An operation with a source ([`op_source`]) whose offset the source has
/// reached already, as the transaction finds it when the request starts,
/// is a replay's and is skipped; afterwards each source's highest offset
/// is kept, in the same transaction ([`WriteTxn::set_source`]). So a
/// stream resumed from its sources' offsets applies each event once. An
/// operation in slots that arrived from another shard at another offset
/// is checked against theirs ([`arrivals`]), which the request, routed by
/// the map of `epoch`, moves on.
fn apply_into(catalog: &[Arc<Schema>], txn: &mut WriteTxn<'_>, ops: &[Json], epoch: u64, arrived: &mut arrivals::Group<'_>) -> Result<Applied, ApiError> {
    let mut a = Applied::default();
    let now = chloedb::timestamp::now();
    // The keys documents of this request wrote, by table.
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    // Each source's offset when the request started, its highest applied
    // here, and the highest it names here.
    let mut sources: Vec<(String, Option<i64>, i64, i64)> = Vec::new();
    // The event last stamped: its operations follow one another.
    let mut stamped = None;
    for op in ops {
        if let Some(m) = op_source(op)? {
            let i = match sources.iter().position(|(n, ..)| *n == m.name) {
                Some(i) => i,
                None => {
                    let stored = txn.source_offset(&m.name)?;
                    sources.push((m.name.clone(), stored, i64::MIN, i64::MIN));
                    sources.len() - 1
                }
            };
            let stored = sources[i].1;
            sources[i].3 = sources[i].3.max(m.offset);
            // An event's operation in slots that arrived at another offset
            // is checked against theirs.
            let reached = match is_mark(op) || arrived.of(txn, &m.name)?.is_empty() {
                true => stored,
                false => {
                    let schema = &**op_schema(catalog, op)?;
                    arrived.threshold(txn, &m.name, slots::slot(&op_place(schema, op)?), stored)?
                }
            };
            if reached.is_some_and(|s| m.offset <= s) {
                a.skipped += !is_mark(op) as u64;
                continue;
            }
            let (_, _, top, _) = &mut sources[i];
            *top = (*top).max(m.offset);
            if m.ts.is_some() && stamped != Some((i, m.offset)) {
                a.stamps.extend(m.ts);
                stamped = Some((i, m.offset));
            }
        }
        if is_mark(op) {
            if !has_source(op) {
                return Err(ApiError::bad("a \"source\" operation needs a source: {\"op\": \"source\", \"source\": {\"name\": ..., \"offset\": n}}"));
            }
            continue;
        }
        let schema = &**op_schema(catalog, op)?;
        let mut tt = txn.table(&schema.table)?;
        let t = &mut tt;
        let kind = op.get("op").and_then(|o| o.as_str()).unwrap_or("");
        let doc = || js::document(schema, op.get("doc").unwrap_or(&Json::Null), now);
        let field = || op.get("field").and_then(|f| f.as_str()).ok_or_else(|| ApiError::bad(format!("{kind} needs a field")));
        let checked = |t: &TableTxn<'_, '_>| -> Result<(), ApiError> {
            match op.get("if_seq_no") {
                None => Ok(()),
                Some(_) if kind == "insert" => Err(ApiError::bad("insert takes no if_seq_no: it fails if the row exists")),
                Some(_) => check_seq_no(t, op, &op_key(schema, op)?),
            }
        };
        checked(t)?;
        // Whether a row had the key before this operation, and whether an
        // earlier operation of this request wrote it.
        let existed = |t: &mut TableTxn<'_, '_>, key: &Value| -> Result<bool, ApiError> {
            let pk = schema.primary_key.map(|pk| schema.fields[pk].name.as_str());
            Ok(match pk {
                Some(pk) => t.value(key, pk)?.is_some(),
                None => false,
            })
        };
        let mut seen_before = |key: &Value| !seen.insert((schema.table.to_ascii_lowercase(), js::to_json(key).to_string()));
        let doc_key = |d: &chloedb::value::Document| schema.primary_key.map(|pk| d[pk].clone());
        // A document written: inserted, replaced, or collapsed onto one
        // this request wrote.
        let mut found = Found::Inserted;
        let wrote = match kind {
            "insert" => {
                let d = doc()?;
                if let Some(k) = doc_key(&d) {
                    seen_before(&k);
                }
                t.insert(d)?;
                true
            }
            // INSERT ... ON CONFLICT DO NOTHING through a coordinator
            // ([`crate::grpc::RECEIPT_PROTOCOL`]): a key a row has, or an
            // earlier operation wrote, writes nothing.
            "insert_if_absent" => {
                let d = doc()?;
                let there = match doc_key(&d) {
                    Some(k) => seen_before(&k) || existed(t, &k)?,
                    None => false,
                };
                if !there {
                    t.insert(d)?;
                }
                !there
            }
            "upsert" => {
                let d = doc()?;
                if let Some(k) = doc_key(&d) {
                    found = Found::of(seen_before(&k), existed(t, &k)?);
                }
                t.upsert(d)?;
                true
            }
            "upsert_if_newer" => {
                let v = op.get("version").and_then(|v| v.as_str()).ok_or_else(|| ApiError::bad("upsert_if_newer needs a version field"))?;
                let d = doc()?;
                if let Some(k) = doc_key(&d) {
                    found = Found::of(seen_before(&k), existed(t, &k)?);
                }
                t.upsert_if_newer(d, v)?
            }
            "delete" => {
                found = Found::Deleted;
                t.delete(&op_key(schema, op)?)?
            }
            "set" => {
                found = Found::Updated;
                let key = op_key(schema, op)?;
                let f = field()?;
                let fi = schema.field_index(f).ok_or_else(|| ApiError::bad(format!("no field {f}")))?;
                t.set(&key, f, js::field_value(schema, fi, op.get("value").unwrap_or(&Json::Null))?)?
            }
            // The transaction's own writes count, other requests' in the
            // same group commit included. With a "doc", a key no row has
            // inserts it, its field plus the value.
            "add" => {
                let raw = op.get("value").unwrap_or(&Json::Null);
                let delta = match (raw.as_i64(), raw.as_f64()) {
                    (Some(x), _) => Value::Int(x),
                    (None, Some(x)) => Value::Float(x),
                    _ => return Err(ApiError::bad("add needs a numeric value")),
                };
                match op.get("doc") {
                    Some(_) => {
                        let d = doc()?;
                        let pk = schema.primary_key.ok_or_else(|| ApiError::bad("writes by key need a primary key"))?;
                        if op.get("key").is_some() && op_key(schema, op)? != d[pk] {
                            return Err(ApiError::bad("add's key and its doc's key differ: give one"));
                        }
                        found = if existed(t, &d[pk])? { Found::Updated } else { Found::Inserted };
                        t.add_or_insert(d, field()?, &delta)?;
                        true
                    }
                    None => {
                        found = Found::Updated;
                        t.add(&op_key(schema, op)?, field()?, &delta)?
                    }
                }
            }
            other => return Err(ApiError::bad(format!("unknown write op {other:?}"))),
        };
        let r = &mut a.receipt;
        match (found, wrote) {
            (Found::Inserted, true) => r.inserted += 1,
            (Found::Replaced, true) => r.replaced += 1,
            (Found::Collapsed, true) => r.collapsed += 1,
            (Found::Updated, true) => r.updated += 1,
            (Found::Deleted, true) => r.deleted += 1,
            // An upsert_if_newer of an older version: kept the row.
            (Found::Inserted | Found::Replaced | Found::Collapsed, false) => {}
            (Found::Updated | Found::Deleted, false) => {
                r.not_found += 1;
                let asked = op.get("if_missing").and_then(|m| m.as_str());
                if asked == Some("error") || (asked.is_none() && op.get("strict").and_then(Json::as_bool) == Some(true)) {
                    return Err(not_found(schema, op, kind));
                }
            }
        }
        if op.get("strict").and_then(Json::as_bool) == Some(true) && wrote && matches!(found, Found::Replaced | Found::Collapsed) {
            let key = op_key(schema, op).map(|k| key_text(&k)).unwrap_or_default();
            let what = match found {
                Found::Replaced => "a row has it already",
                _ => "an earlier operation of this request wrote it",
            };
            return Err(ApiError::bad(format!("strict: {kind} of key {key} in {} would not insert a new row: {what}; nothing was written", schema.table)));
        }
        // A key written twice leaves one row.
        a.rows += (wrote && found != Found::Collapsed) as u64;
    }
    for (name, stored, top, seen) in sources {
        let kept = match top > i64::MIN && stored.is_none_or(|s| top > s) {
            true => {
                txn.set_source(&name, top, now)?;
                Some(top)
            }
            false => stored,
        };
        arrived.after(txn, &name, epoch, seen, kept, now)?;
    }
    Ok(a)
}


#[cfg(test)]
mod tests {
    use super::*;

    // A batch's part for the shard slots move to, routed by the map before
    // their flip, reached it after the flip and moved its offset past an
    // event of the slots that the shard they left still held (theirs and
    // its own were equal at the flip, so it kept no arrival): when the
    // event came by the new map, it was skipped as a replay, and lost.
    // Both on a shard that holds slots and on one that held none.
    #[test]
    fn a_late_request_of_the_old_map_does_not_hide_an_arrived_event() {
        let schema = Schema::from_json(r#"{"table": "events", "key": "id", "fields": [{"name": "id", "type": "keyword"}]}"#).unwrap();
        let key = (0..).map(|i| format!("e{i}")).find(|k| SlotMap::even(2, 1).owner(slots::slot(&Value::Str(k.clone()))) == 0).unwrap();
        let y = slots::slot(&Value::Str(key.clone()));
        let mark = |o: i64| json!({"op": "source", "source": {"name": "s", "offset": o}});
        let event = json!({"op": "upsert", "table": "events", "doc": {"id": key}, "source": {"name": "s", "offset": 11}});
        for base in [SlotMap::even(2, 1), SlotMap::even(1, 1).with_replicas(vec![vec!["http://a".into()]]).unwrap().add_shard(vec!["http://new".into()]).unwrap()] {
            let dir = tempfile::tempdir().unwrap();
            let db = Db::create(&dir.path().join("t.strata"), schema.clone(), chloedb::storage::DbOptions::default()).unwrap();
            let old = base.start_move(&[y], 1).unwrap();
            let new = old.flip_move_with(std::collections::BTreeMap::from([("s".to_string(), 10)])).unwrap();
            apply_requests(&db, &[(&[mark(10)], old.epoch)], None).unwrap();
            write_flip(&db, &[], &old, &new, 1).unwrap();
            // The late part: no events of the slots, the batch's offset.
            apply_requests(&db, &[(&[mark(11)], old.epoch)], None).unwrap();
            // The event the shard they left held, by the new map.
            let (a, _) = apply_requests(&db, &[(&[event.clone(), mark(11)], new.epoch)], None).unwrap();
            assert_eq!((a[0].rows, a[0].skipped), (1, 0), "held none: {}", base.count(1) == 0);
            // Once: sent again, it is skipped, and the arrival is gone.
            let (a, _) = apply_requests(&db, &[(&[event.clone(), mark(11)], new.epoch)], None).unwrap();
            assert_eq!((a[0].rows, a[0].skipped), (0, 1));
            let left: Vec<(String, i64)> = db.sources().unwrap().into_iter().map(|s| (s.name, s.offset)).collect();
            assert_eq!(left, vec![("s".to_string(), 11)]);
        }
    }

    // A table created after the server started, naming a model the models
    // directory lacked, did not wake `keep_models`, which then fetched only
    // at start: the field had no model, and its rows no vectors, until the
    // server was started again.
    #[tokio::test]
    async fn a_table_naming_a_model_to_fetch_wakes_the_fetcher() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create_empty(&dir.path().join("t.strata"), chloedb::storage::DbOptions::default()).unwrap());
        let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
        let mut svc = Service::new(Backend::Local(db.clone()), (*db.schema()).clone(), agent, 16, Duration::from_secs(10));
        svc.open_models(&dir.path().join("models"));
        let svc = Arc::new(svc);
        let mut changed = svc.models_changed.subscribe();
        changed.borrow_and_update();
        let ddl = "CREATE TABLE listings (id KEYWORD PRIMARY KEY, title TEXT, emb VECTOR OPTIONS(embed = 'title', model = 'potion-base-8M'))";
        svc.sql(&Principal::open(), ddl, &Params::new(), After::Any, 0).await.unwrap();
        assert!(changed.has_changed().unwrap(), "the fetcher was not woken");
        let models = svc.models();
        assert!(models.len() == 1 && models[0].model.as_ref().is_err_and(|e| e.contains("fetch it first")), "{:?}", models.iter().map(|m| m.model.as_ref().err()).collect::<Vec<_>>());
        // Opening it again after a fetch that failed wakes it no more.
        changed.borrow_and_update();
        svc.reopen_models();
        assert!(!changed.has_changed().unwrap());
    }

    // Past saturation every query waited for a slot until its deadline, so
    // a server under ten times its capacity answered all of them seconds
    // late. With an admission wait, a client's query that finds no slot in
    // time is refused at once (429), while a shard's call from its
    // coordinator still waits until its deadline.
    #[tokio::test]
    async fn a_query_finding_no_slot_within_the_admission_wait_is_refused_early() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create_empty(&dir.path().join("t.strata"), chloedb::storage::DbOptions::default()).unwrap());
        let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
        let deadline = Duration::from_secs(10);
        let mut svc = Service::new(Backend::Local(db.clone()), (*db.schema()).clone(), agent, 1, deadline);
        assert_eq!(svc.admission_budget(deadline, false), deadline, "no wait set: until the deadline");
        svc.admission_wait = Some(Duration::from_millis(50));
        assert_eq!(svc.admission_budget(deadline, false), Duration::from_millis(50));
        assert_eq!(svc.admission_budget(deadline, true), deadline, "a shard's call waits until its deadline");
        assert_eq!(svc.admission_budget(Duration::from_millis(20), false), Duration::from_millis(20), "never past the deadline");
        let held = svc.admission.clone().acquire_owned().await.unwrap();
        let t0 = Instant::now();
        assert!(svc.admit(svc.admission_budget(deadline, false)).await.is_none());
        assert!(t0.elapsed() < Duration::from_secs(2), "refused after {:?}", t0.elapsed());
        drop(held);
        assert!(svc.admit(svc.admission_budget(deadline, false)).await.is_some(), "a free slot is taken at once");
    }

    // A primary fenced by a call of the new primary as it kept its vote
    // for it wrote its replication file twice at once, through one new
    // file: one rename failed (no such file), and the file was left with
    // the end of the longer write after the shorter, which no later step
    // down or start could read.
    #[test]
    fn json_saved_at_once_by_two_threads_is_one_or_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kept.json");
        let bodies: Vec<Json> = (0..8).map(|i| json!({"i": i, "pad": "x".repeat(1 + i * 300)})).collect();
        for _ in 0..50 {
            std::thread::scope(|s| {
                for b in &bodies {
                    s.spawn(|| save_json(&path, b).unwrap());
                }
            });
            let j: Json = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(bodies.contains(&j), "{j}");
        }
    }

    // OPS-6: a panic in the writer thread ended it, and every later write
    // failed with health still "ok". The writer now catches it; the
    // requests of the batch it was applying are answered as they drop.
    #[test]
    fn a_write_dropped_unanswered_gets_an_error() {
        let keys = Arc::new(idem::Keys::default());
        assert_eq!(keys.begin("k", 1), idem::Begin::New);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let state = WriterState::default();
        let q = Queued { job: WriteJob::Ops(Vec::new()), deadline: Instant::now(), reply: Some(tx), key: Some(("k".into(), 1)), keys: keys.clone(), queued: InQueue::start(&state, 1), pass: None, mark: None, epoch: 0, term: None };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = q;
            panic!("a bug while applying a write");
        }));
        assert!(r.is_err());
        assert_eq!(rx.try_recv().unwrap().unwrap_err().status, 500);
        // It did not commit, so a retry with its key applies it.
        assert_eq!(keys.begin("k", 1), idem::Begin::New);
        assert_eq!(state.queued.load(Ordering::Relaxed), 0);
    }
}

/// The table a `{"vector_of"}` or `{"embed"}` parameter (`form`)
/// resolves against ([`Service::params_for`]): `given`, else the one of the
/// tables its statement names (`named`, lowercase) that `has` such a
/// field, refused when several do; else the first of `order` (the
/// database's) that has one and `who` may read. `None`: none has one.
fn param_table(given: Option<&str>, named: &[String], order: &[String], who: &Principal, has: &dyn Fn(&str) -> bool, form: &str) -> Result<Option<String>, ApiError> {
    if let Some(t) = given {
        return Ok(Some(t.to_string()));
    }
    let of_statement: Vec<&String> = order.iter().filter(|t| named.iter().any(|n| n.eq_ignore_ascii_case(t)) && has(t)).collect();
    match of_statement.as_slice() {
        [t] => return Ok(Some(t.to_string())),
        [] => {}
        many => {
            let names: Vec<&str> = many.iter().map(|t| t.as_str()).collect();
            return Err(ApiError::bad(format!("{{\"{form}\"}} names no table, and the statement reads {} with such a field: add \"table\" to the parameter", names.join(" and "))));
        }
    }
    Ok(order.iter().find(|t| has(t) && who.table_scope(t).is_some_and(|s| s >= Scope::Read)).cloned())
}

/// Whether `text` holds `word`, in any case.
fn contains_word(text: &str, word: &str) -> bool {
    text.as_bytes().windows(word.len()).any(|w| w.eq_ignore_ascii_case(word.as_bytes()))
}
