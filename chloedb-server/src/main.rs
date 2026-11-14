//! `chloedb-server`: ChloeDB over HTTP JSON and gRPC, with a query UI.
//!
//! ```text
//! chloedb-server --demo 20000 --db products.chloedb     # a demo catalogue, UI at http://127.0.0.1:7700
//! chloedb-server --db my.chloedb                        # an existing database, or a new one with no tables yet
//! CHLOEDB_TOKEN=... chloedb-server --db my.chloedb --listen 0.0.0.0:7700  # beyond loopback: a token
//! chloedb-server --shards http://a:7701,http://b:7701   # a coordinator over shard servers
//! chloedb-server --shards 'http://a0:7701|http://a1:7701,http://b0:7701|http://b1:7701'   # two replicas each
//! chloedb-server --db a0.chloedb --seq-no --replicas http://a1:7701  # a primary shipping its writes to a replica
//! chloedb-server --db a1.chloedb --seq-no --replica                   # and that replica
//! chloedb-server --db new.chloedb --schema new.json --copy-from my.chloedb  # re-embed into a new schema
//! chloedb-server fetch-model potion-retrieval-32M       # download an embedding model
//! chloedb-server import --db my.chloedb --schema my.json rows.jsonl more.csv  # load files
//! chloedb-server import --db shop.chloedb shop.sqlite   # a SQLite database's tables
//! chloedb-server export --db shop.chloedb shop-copy.sqlite  # and back to SQLite
//! chloedb-server upgrade my.chloedb                     # copy a database from before field ids into this version's format
//! ```

use std::path::PathBuf;

/// Every allocation counted: the heap's size, its rate and where it is
/// allocated (`chloedb_server::heap`).
#[global_allocator]
static ALLOC: chloedb_server::heap::Counting = chloedb_server::heap::Counting;

use clap::{Parser, Subcommand};
use chloedb_server::auth::{self, Access, Secret, Tls};
use chloedb_server::obs;
use chloedb_server::server::{self, Options};

/// `println!` for a command's answer, which a closed pipe ends quietly
/// ([`chloedb_server::print_out`]).
macro_rules! out {
    ($($t:tt)*) => {
        chloedb_server::print_out(&format!($($t)*))
    };
}

#[derive(Parser)]
#[command(version, about = "ChloeDB over HTTP JSON and gRPC, with a query UI and an agent", args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The database to serve. Created if missing: with --schema's table or
    /// --demo's, else with no tables (CREATE TABLE and imports add them).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Schema of a new database: a JSON file, or the JSON itself
    /// (`--schema '{"table": "t", "key": "id", "fields": [...]}'`).
    #[arg(long)]
    schema: Option<PathBuf>,
    /// Create the demo product catalogue with this many products.
    #[arg(long)]
    demo: Option<usize>,
    /// With --demo: keep only the products of shard I of N ("I/N"), those
    /// in its slots when N shards hold equal runs of slots.
    #[arg(long)]
    demo_shard: Option<String>,
    /// Fill the new database (--db with --schema) with this database's rows,
    /// field by field by name; vector fields whose embedding changed are
    /// embedded anew.
    #[arg(long)]
    copy_from: Option<PathBuf>,
    /// Where embedded fields' models are (see `fetch-model`).
    #[arg(long, env = "CHLOEDB_MODELS")]
    models: Option<PathBuf>,
    /// Leave rows stored while their embedded field had no model open
    /// without vectors. By default they get them once the model opens.
    #[arg(long)]
    no_backfill: bool,
    /// Do not download the models ChloeDB knows (see `fetch-model`) that an
    /// embedded field names and --models lacks.
    #[arg(long)]
    no_fetch_models: bool,
    /// Create a new database as a directory of segment files.
    #[arg(long)]
    directory: bool,
    /// Open a database whose write-ahead log was damaged after it was
    /// durable, dropping the transactions from the damage on (the log is
    /// copied to `<log>.damaged` first). Without it the server refuses to
    /// start on such a database.
    #[arg(long)]
    wal_drop_damaged: bool,
    /// Open a damaged database that the server otherwise refuses to start
    /// on because opening it loses acknowledged writes (its newest version
    /// fails a check and the write-ahead log no longer holds what it held,
    /// or the log's header is damaged): keeps a copy of the damaged file
    /// (`<db>.damaged-<time>`, `<db>-wal.damaged`), opens what is left,
    /// and reports the loss in `/v1/health` and `/v1/admin/check`.
    /// Implies --wal-drop-damaged. Restoring a backup loses less.
    #[arg(long)]
    recover: bool,
    /// Check every segment of the database against its checksum before
    /// serving (reads the whole database once). A damaged newest version
    /// is then skipped for the one before it, as when its manifest is
    /// damaged.
    #[arg(long)]
    verify_checksums: bool,
    /// Number the database's writes, for good: each row then has the
    /// sequence number of its last write (`seq_no()` in a SELECT), and a
    /// write operation can apply only while a row's number is the one it
    /// read (`if_seq_no`). The database then needs this version of
    /// ChloeDB or a later one.
    #[arg(long)]
    seq_no: bool,
    /// Ship this database's writes to these replicas, as their primary
    /// (comma-separated gRPC URLs of servers started with --replica), so
    /// that each holds the same rows under the same sequence numbers.
    /// Needs --seq-no. With --replica: the shard's other copies (its
    /// primary and the other replicas), which it ships to once promoted;
    /// a promotion asked for with an API key ships only to these.
    #[arg(long, value_delimiter = ',')]
    replicas: Vec<String>,
    /// Serve as a replica: apply the writes a primary ships here (its
    /// --replicas names this server's gRPC URL), and refuse writes from
    /// clients. Needs --seq-no, and the rows the primary had when it
    /// started numbering (the same --demo, or a checkpoint of it).
    #[arg(long)]
    replica: bool,
    /// Start as the primary of a new term, above every term the database
    /// knows, shipping to --replicas (if any): needed when the database
    /// was a replica, or another primary replaced it. A running replica is
    /// promoted with `chloedb-server promote` instead.
    #[arg(long, conflicts_with = "replica")]
    promote: bool,
    /// With --replicas: the copies that hold a write before it is
    /// answered: all (the primary and every replica in sync), majority (of
    /// all copies), or primary.
    #[arg(long, default_value = "all")]
    write_acks: String,
    /// With --replicas or --replica: the latest transactions kept in
    /// memory to ship, in MiB. A replica further behind is sent a copy of
    /// the primary.
    #[arg(long, default_value_t = 64)]
    ship_log_mb: usize,
    /// With --replicas: compare each replica's rows with the primary's
    /// every this many seconds, and send a replica whose rows differ a
    /// copy of the primary (0: only when asked, POST /v1/admin/verify).
    #[arg(long, default_value_t = 600)]
    verify_every_s: u64,
    /// With --replicas or --replica, in a cluster (servers holding a
    /// cluster map): a replica that hears nothing from its primary for this
    /// many milliseconds asks its shard's copies to elect it primary, and a
    /// primary that does not hear from a majority of its copies for most of
    /// it serves no reads (0: no automatic failover; promote by hand).
    #[arg(long, default_value_t = 0)]
    failover_after_ms: u64,
    /// How long a write's idempotency key is remembered, in seconds (at
    /// least 60): a retry within it is answered as the first attempt was,
    /// and a write through a coordinator that failed partway across shards
    /// is finished by sending it again with its key within it. Each key a
    /// write leaves is a row of `_sources` for that long.
    #[arg(long, default_value_t = 3600)]
    idempotency_ttl_s: u64,
    /// On a database server: read every segment against its checksum every
    /// this many seconds, while it serves, to find data damaged on disk
    /// before a query or a failover does (0: only when asked, POST
    /// /v1/admin/scrub). The first scrub starts within ten minutes.
    #[arg(long, default_value_t = 86_400)]
    scrub_every_s: u64,
    /// The MiB a second a scheduled scrub reads at most.
    #[arg(long, default_value_t = 64)]
    scrub_mib_s: u64,
    /// Coordinate these shard servers (comma-separated gRPC URLs; replicas
    /// of one shard joined by '|'). Servers of protocol 9 hold the cluster
    /// map, which names every shard's servers: the coordinator then routes
    /// by the newest map they hold, whatever servers this lists.
    #[arg(long, value_delimiter = ',')]
    shards: Vec<String>,
    /// With --shards: ask another replica of a shard when one has not
    /// answered after this many milliseconds.
    #[arg(long, default_value_t = 100)]
    hedge_ms: u64,
    /// With --shards: give up on a shard after this many milliseconds (0:
    /// at the query's deadline).
    #[arg(long, default_value_t = 0)]
    shard_timeout_ms: u64,
    /// With --shards: how the replica of a shard asked first for a read is
    /// chosen: round-robin (each in turn) or least-cost (in turn, unless
    /// another costs clearly less by its recent round trips, calls in
    /// flight, errors and reported load).
    #[arg(long, default_value = "round-robin", value_parser = ["round-robin", "least-cost"])]
    route: String,
    /// With --shards: hedge a query after the replica asked has taken
    /// longer than it usually does (its recent average round trip plus six
    /// deviations; 2 ms at least, --hedge-ms at most), instead of after
    /// --hedge-ms.
    #[arg(long)]
    hedge_adaptive: bool,
    /// With --shards and --route least-cost: route a read that must see a
    /// write (min_seq_nos) by each replica's round trips of such reads,
    /// unless it is known to hold the write already.
    #[arg(long)]
    prefer_caught_up: bool,
    /// With --shards: answer without shards that have no replica answering,
    /// flagged "partial" (a request's "partial" overrides it).
    #[arg(long)]
    partial: bool,
    /// With --shards: terms whose BM25 statistics the coordinator keeps
    /// per shard copy, as of the copy's version, so that a BM25 query whose
    /// terms are all kept skips the statistics round (0: none kept; every
    /// such query asks every shard first).
    #[arg(long, default_value_t = chloedb_server::coord::STATS_CACHE_TERMS)]
    stats_cache_terms: usize,
    /// With --shards: send a write to every server of a shard whose servers
    /// do not replicate (none started with --replicas or --replica), each
    /// taking it on its own. Without it such a shard serves reads only: its
    /// copies would diverge, and nothing would ever make them agree again.
    #[arg(long)]
    write_every_copy: bool,
    /// With --shards: the StrataCluster (namespace/name) that runs this
    /// coordinator on Kubernetes, read and scaled through the API server as
    /// the pod's service account (the operator sets it).
    #[arg(long, env = "CHLOEDB_KUBE_CLUSTER")]
    kube_cluster: Option<String>,
    /// With --db, --grpc and --shards: this shard server also coordinates
    /// --shards, serving the coordinator's HTTP API at this address, in
    /// the same process. The coordinator calls this server's own shard
    /// (--local-shard) in-process, without a connection or HTTP/2, and the
    /// others over the network.
    #[arg(long)]
    coordinator_listen: Option<String>,
    /// With --coordinator-listen: the URL of --shards that is this server's
    /// own gRPC address (http://strata-0.strata:7701).
    #[arg(long)]
    local_shard: Option<String>,
    /// HTTP address (API, metrics, UI).
    #[arg(long, default_value = "127.0.0.1:7700")]
    listen: String,
    /// gRPC address ("off" to disable).
    #[arg(long, default_value = "127.0.0.1:7701")]
    grpc: String,
    /// Also serve an OpenSearch-compatible API (indexes, documents, _bulk,
    /// _search with the Query DSL and aggregations) on this address, for
    /// applications written for OpenSearch or Elasticsearch 7: the same
    /// data, credentials and TLS as the HTTP API (a key or the token as a
    /// Bearer token, or as Basic authentication's password).
    #[arg(long, env = "CHLOEDB_OPENSEARCH_LISTEN")]
    opensearch_listen: Option<String>,
    /// The version the OpenSearch API's root reports: OpenSearch 2's by
    /// default; 7.10.2 for clients of Elasticsearch 7 that check it.
    #[arg(long, env = "CHLOEDB_OPENSEARCH_VERSION", default_value = chloedb_server::opensearch::VERSION_NUMBER)]
    opensearch_version: String,
    /// Require "Authorization: Bearer TOKEN" on the API and on gRPC (the
    /// UI asks for it). Needed to listen beyond loopback.
    #[arg(long, env = "CHLOEDB_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Listen beyond loopback without --token: anyone who can reach the
    /// ports can read and write.
    #[arg(long)]
    no_token: bool,
    /// Also answer HTTP requests for this host name (repeatable). On
    /// loopback the server answers only to localhost, 127.0.0.1 and ::1, so
    /// that a web page cannot reach it by DNS rebinding.
    #[arg(long)]
    allow_host: Vec<String>,
    /// Serve HTTP and gRPC over TLS with this certificate chain (PEM).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// The private key of --tls-cert (PEM).
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// The token the servers of a cluster send each other: a coordinator
    /// its shards, a primary its replicas. Give every server of the
    /// cluster the same one. With it, only it may make the calls between
    /// servers (replication, slots, keys), and --token is an admin's
    /// credential that may not; clients get API keys. Without it, --token
    /// plays both parts.
    #[arg(long, env = "CHLOEDB_CLUSTER_TOKEN", hide_env_values = true)]
    cluster_token: Option<String>,
    /// With --shards or --replicas: a CA certificate (PEM) for https://
    /// shards or replicas, besides the public roots.
    #[arg(long)]
    shard_ca: Option<PathBuf>,
    /// With --shards: the slot map to route writes by, which the shards are
    /// given (JSON, each shard's slots as [start, end) ranges of the 4,096:
    /// {"epoch": 2, "shards": [[[0, 2048]], [[2048, 4096]]]}). Default:
    /// equal runs of slots, epoch 1.
    #[arg(long)]
    slot_map: Option<PathBuf>,
    /// Default query deadline.
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,
    /// The longest deadline a request's `timeout_ms` may ask for.
    #[arg(long, default_value_t = 60000)]
    max_timeout_ms: u64,
    /// Queries running at once; more wait for a slot until their deadline.
    #[arg(long, default_value_t = 64)]
    max_concurrent: usize,
    /// How long a client's query waits for a slot (--max-concurrent)
    /// before it is refused with 429 and Retry-After, instead of waiting
    /// until its deadline (0: until its deadline). A coordinator's calls to
    /// its shards always wait until their deadline.
    #[arg(long, default_value_t = 0)]
    admission_wait_ms: u64,
    /// The most rows a query may reach, OFFSET included: a larger LIMIT
    /// plus OFFSET, or a query without a LIMIT that matches more, fails
    /// (0: no limit).
    #[arg(long, default_value_t = 10_000)]
    max_result_window: usize,
    /// The rows the open scrolls (`page_size`) of ordered queries, groups
    /// and joins hold between them, or their places: a scroll that would
    /// hold more is refused (0: only scrolls without ORDER BY, groups or
    /// joins, which hold none).
    #[arg(long, default_value_t = chloedb_server::service::MAX_SCROLL_ROWS)]
    max_scroll_rows: usize,
    /// The most groups a GROUP BY may make before the query fails (0: no
    /// limit).
    #[arg(long, default_value_t = 65_536)]
    max_groups: usize,
    /// The most rows a cross-encoder's rerank phase may score, on a
    /// database or a coordinator: a query whose phase asks for more fails
    /// (0: no limit). Twenty abstracts take ms-marco-MiniLM-L-6-v2 about
    /// 1 s at the median and at most 1.3 s at p99 on four cores in f32, a
    /// quarter of the default deadline; a hundred take about all of it (its
    /// int8 export, 0.4 s and 2 s).
    #[arg(long, default_value_t = 20)]
    max_rerank: usize,
    /// Rows a query may ask a language model about with `llm()`, on a
    /// database or a coordinator: a query that returns more, or whose
    /// `ORDER BY llm(...)` ranks more, fails before asking. Each is a
    /// request to the model, of about a second, eight at once.
    #[arg(long, default_value_t = 100)]
    llm_rows: usize,
    /// Requests this server makes to language models for `llm()` a minute,
    /// over every query (answers it keeps in memory are free): a query
    /// that would make more fails with 503 (0: no limit).
    #[arg(long, default_value_t = 600)]
    llm_per_minute: u64,
    /// Model calls an API key may make a day (UTC) with this server's own
    /// credentials: /v1/agent and the on-call agent a call a step,
    /// /v1/judge a call a row, llm() a call a question it asks (answers
    /// kept in memory are free); past it they answer 429 until the next
    /// day. The --token, the cluster's token and callers that bring their
    /// own api_key are not counted (0: no limit).
    #[arg(long, env = "CHLOEDB_MODEL_CALLS_PER_KEY_DAY", default_value_t = chloedb_server::budget::DEFAULT_PER_DAY)]
    model_calls_per_key_day: u64,
    /// Memory the running queries may hold together, in MiB: their groups,
    /// candidate rows, answers and joins; a query that needs more fails
    /// with 429 (default: a third of the cgroup memory limit, or of RAM;
    /// 0: no limit).
    #[arg(long)]
    query_memory_mb: Option<u64>,
    /// Memory for filters kept between queries, in MiB: a filter a segment
    /// sees twice is kept until less recently used ones need the room
    /// (default: 64; 0: none kept).
    #[arg(long)]
    filter_cache_mb: Option<usize>,
    /// Memory for answers kept between queries, in MiB: a read query asked
    /// again on the same version of the data is answered from them, without
    /// running (default: 64; 0: none kept). Every write moves the version on.
    #[arg(long)]
    answer_cache_mb: Option<usize>,
    /// The longest SQL statement a request may send, in bytes; a longer
    /// one is refused with a 400 query error before it is parsed (0: no
    /// limit). A long list of values passed as a parameter does not count.
    #[arg(long, default_value_t = chloedb::query::parse::STATEMENT_BYTES)]
    max_statement_bytes: usize,
    /// The most elements (words, values and symbols) a SQL statement a
    /// request sends may hold, as ClickHouse's `max_ast_elements` bounds
    /// (0: no limit).
    #[arg(long, default_value_t = chloedb::query::parse::STATEMENT_ELEMENTS)]
    max_statement_elements: usize,
    /// Write requests the server holds at once (embedding, waiting for the
    /// writer, or being applied); more are refused with 429 (0: no limit).
    #[arg(long, default_value_t = 1024)]
    max_queued_writes: usize,
    /// The largest body a write or an import may send, in MiB (other
    /// requests take up to 16 MiB).
    #[arg(long, default_value_t = 256)]
    max_body_mb: usize,
    /// How long the tables OpenTelemetry logs make (`otel_logs`) keep
    /// their rows, as `30 days` or `12h`: older rows are dropped, a window
    /// of segments at a time (default: kept for ever). Tables already
    /// there keep their own retention.
    #[arg(long, value_parser = |s: &str| chloedb::schema::Retain::parse_period(s).map_err(|e| e.to_string()))]
    otlp_retain: Option<i64>,
    /// The agent's default model.
    #[arg(long, default_value = chloedb_server::agent::DEFAULT_MODEL)]
    model: String,
    /// The Claude API's base URL.
    #[arg(long, env = "ANTHROPIC_BASE_URL", default_value = "https://api.anthropic.com")]
    api_base: String,
    /// Synonyms for `synonyms => 1` searches, one rule per line in Solr's
    /// format (`laptop, notebook` or `tv => television`); the UI edits it.
    #[arg(long)]
    synonyms: Option<PathBuf>,
    /// A directory for checkpoints (consistent copies of the database,
    /// taken while it serves) written by `POST /v1/admin/checkpoint`.
    #[arg(long)]
    backup_dir: Option<PathBuf>,
    /// A directory for slices (a table's rows by map tile, for devices to
    /// search near their user: `chloedb::slice`), written by `POST
    /// /v1/admin/slices` and served at `GET /v1/slices/<table>/...`.
    #[arg(long)]
    slices: Option<PathBuf>,
    /// Every this many seconds, rebuild the slices whose table changed
    /// since they were built, so that devices find a fresh one (0: only
    /// when asked, by `POST /v1/admin/slices`).
    #[arg(long, default_value_t = 0)]
    slices_rebuild_s: u64,
    /// What writes may fetch for MEDIA fields, comma-separated: directories
    /// (`file:/data/media`) and hosts fetched over HTTPS
    /// (`cdn.example.com`, `*.example.com`; `http://host:port` only spelled
    /// out). Nothing by default: writes then give the bytes or the whole
    /// value.
    #[arg(long, value_delimiter = ',')]
    media_fetch: Vec<String>,
    /// The largest media file a write fetches or gives, in bytes.
    #[arg(long, default_value_t = chloedb::media::DEFAULT_MAX_BYTES)]
    media_max_bytes: u64,
    /// Where the bytes a write gives a MEDIA field without a URI are kept,
    /// as DIR/<sha256> (the value's uri is then media:<sha256>).
    #[arg(long)]
    media_store: Option<PathBuf>,
    /// How long a media fetch may take, in seconds.
    #[arg(long, default_value_t = 30)]
    media_timeout_s: u64,
    /// Keep segments cold on object storage, read through a local cache: an
    /// `s3://bucket/prefix/` URL (configured as `import` reads them) or a
    /// directory. The database must be a directory (`--directory`); once a
    /// segment is cold, it opens only with its store.
    #[arg(long)]
    cold_store: Option<String>,
    /// The cold tier's cache of fetched parts, in GiB (segments' headers and
    /// keys apart, which it always keeps).
    #[arg(long, default_value_t = 8.0)]
    cold_cache_gb: f64,
    /// Keep the newest segments local up to this many GiB, and make the
    /// older ones cold (default: none goes cold by itself).
    #[arg(long)]
    cold_keep_local_gb: Option<f64>,
    /// Segments smaller than this stay local, in MiB.
    #[arg(long, default_value_t = 16)]
    cold_min_segment_mb: u64,
    /// The cold tier's local cache directory (default: `cold` in the
    /// database's directory).
    #[arg(long)]
    cold_cache_dir: Option<PathBuf>,
    /// Merges that may run at once (default: half the cores, 1 to 4).
    #[arg(long)]
    merge_threads: Option<usize>,
    /// Memory the running merges may use together, in MiB (default: half
    /// of the cgroup memory limit, or of RAM).
    #[arg(long)]
    merge_memory_mb: Option<u64>,
    /// How fast merges may write, in MiB/s (default: no limit).
    #[arg(long)]
    merge_write_mb_per_sec: Option<u64>,
    /// The nice value merges run at, 0 to 19, on Linux (default 10): a
    /// merge gets the cores reads and writes leave idle, and a tenth of a
    /// busy one. 0 runs them at the priority of reads and writes.
    #[arg(long)]
    merge_nice: Option<i32>,
    /// Log queries and writes that take this many milliseconds or more,
    /// with their SQL (0: none).
    #[arg(long, default_value_t = 1000)]
    slow_ms: u64,
    /// The API keys' file (default: DB.keys.json beside --db; a
    /// coordinator keeps its keys in memory without it, and takes them
    /// from its shards when it starts). Keys have scopes (feed, read, write,
    /// admin), are made in the UI or with POST /v1/admin/keys, and are
    /// passed on to the cluster's other servers.
    #[arg(long)]
    keys: Option<PathBuf>,
    /// Append the requests that need the admin scope, and refused ones, to
    /// this file as JSON lines (the latest are kept in memory either way:
    /// GET /v1/admin/audit).
    #[arg(long)]
    audit_log: Option<PathBuf>,
    /// The AWS credentials the command center's cloud view saves (default:
    /// DB.aws.json beside --db, readable by this user only; a coordinator
    /// keeps them in memory without it). Without saved ones, the view uses
    /// AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY and AWS_SESSION_TOKEN.
    #[arg(long)]
    aws_file: Option<PathBuf>,
    /// The DigitalOcean token the command center's cloud view saves
    /// (default: DB.digitalocean.json beside --db, readable by this user
    /// only; a coordinator keeps it in memory without it). Without a saved
    /// one, the view uses DIGITALOCEAN_TOKEN or DIGITALOCEAN_ACCESS_TOKEN.
    #[arg(long)]
    digitalocean_file: Option<PathBuf>,
    /// The clusters the cloud view made, and where each one is (default:
    /// DB.cloud.json beside --db; a coordinator keeps them in memory
    /// without it). No secrets: the clouds' keys stay in their own files.
    #[arg(long)]
    cloud_file: Option<PathBuf>,
    /// The book of connections to other clusters that this server's command
    /// center reaches (default: DB.hub.json beside --db). Their keys are
    /// kept in the macOS Keychain or the Secret Service where there is one,
    /// else in DB.hub-keys.json (mode 600); CHLOEDB_HUB_SECRETS=file keeps
    /// them in the file anyway.
    #[arg(long)]
    hub_file: Option<PathBuf>,
    /// Who this hub's calls to other clusters say they come from (default:
    /// $USER@host), in each cluster's audit log.
    #[arg(long)]
    hub_device: Option<String>,
    /// The alert rules' file, JSON (created when the rules are first
    /// edited; default rules in memory without it).
    #[arg(long)]
    alerts: Option<PathBuf>,
    /// Post each alert that fires or resolves to this URL, as
    /// {"text": ..., "alert": {...}} (a Slack incoming webhook takes it).
    #[arg(long)]
    alert_webhook: Option<String>,
    /// Keep only the types of queries' parameters (not their values) in
    /// the running and finished queries, statements and the log store.
    #[arg(long, env = "CHLOEDB_REDACT_PARAMS")]
    redact_params: bool,
    /// What to log: a level (error, warn, info, debug, trace), or levels
    /// per module ("info,chloedb=debug"). debug logs every request.
    #[arg(long, env = "CHLOEDB_LOG", default_value = "info")]
    log: String,
    /// How to write log lines: text, or json (one object per line).
    #[arg(long, env = "CHLOEDB_LOG_FORMAT", default_value = "text")]
    log_format: String,
    /// Where the server keeps its log lines and the queries it answered, a
    /// ChloeDB database of its own searched with SQL (POST /v1/logs/query,
    /// the UI's Logs): a path, or "off" (default: DB.logs beside --db; a
    /// coordinator keeps none unless given one, and asks its shards').
    #[arg(long)]
    log_store: Option<String>,
    /// How long the log store keeps rows, as "7 days", "12h" or "2 weeks".
    #[arg(long, default_value = "7 days")]
    log_retain: String,
    /// The most the log store takes on disk, in MiB; past it the oldest
    /// rows are deleted (0: no limit).
    #[arg(long, default_value_t = 1024)]
    log_store_mb: u64,
    /// Queries a second the log store keeps besides the slow and failed
    /// ones, which it always keeps (0: only those).
    #[arg(long, default_value_t = 200)]
    log_queries: u64,
    /// This server's name in the log stores' rows (`node`), as a
    /// coordinator shows its servers' logs (default: the host's name and
    /// the HTTP port).
    #[arg(long, env = "CHLOEDB_NODE_NAME")]
    node_name: Option<String>,
    /// The image repository whose versions the command center offers
    /// (`GET /v1/admin/kube/versions`) when no StrataCluster names one.
    #[arg(long, env = "CHLOEDB_IMAGE_REPOSITORY")]
    image_repository: Option<String>,
    /// Once told to stop (SIGTERM, Ctrl-C), serve on this many
    /// milliseconds with /readyz answering 503, so that a load balancer
    /// (a Kubernetes Service) sends no more clients before the server
    /// stops taking connections; then end the requests under way and stop.
    #[arg(long, env = "CHLOEDB_DRAIN_MS", default_value_t = 0)]
    drain_ms: u64,
    /// Once the server stops taking connections, how long the requests
    /// under way have to end (a follower's feed ends at once) before it
    /// stops anyway: keep the drain time and this within the time a
    /// supervisor waits (30 s in Kubernetes).
    #[arg(long, env = "CHLOEDB_STOP_GRACE_MS", default_value_t = 20_000)]
    stop_grace_ms: u64,
    /// The connections each listener (HTTP, OpenSearch, gRPC) holds open
    /// at once; past it, new ones wait to be accepted, but for a few kept
    /// for /livez, /readyz and /v1/health (0: three quarters of the
    /// open-file limit, for the listeners together).
    #[arg(long, env = "CHLOEDB_MAX_CONNECTIONS", default_value_t = 0)]
    max_connections: usize,
    /// The connections one client address may hold of a listener's; a
    /// client past it is closed at once (0: half of --max-connections).
    /// Behind a proxy that keeps few connections to the server, the
    /// default does; one that opens a connection per client may need
    /// more.
    #[arg(long, env = "CHLOEDB_MAX_CONNECTIONS_PER_CLIENT", default_value_t = 0)]
    max_connections_per_client: usize,
    /// The longest a client may take to send a request's headers, and the
    /// longest an idle HTTP/1 connection stays open, in seconds.
    #[arg(long, env = "CHLOEDB_HEADER_TIMEOUT_SECS", default_value_t = 30)]
    header_timeout_secs: u64,
    /// The longest any connection (HTTP/2 and gRPC too) stays open with
    /// no request under way, in seconds (0: no limit).
    #[arg(long, env = "CHLOEDB_IDLE_TIMEOUT_SECS", default_value_t = 300)]
    idle_timeout_secs: u64,
    /// The longest a request body may pause, in seconds; one that has
    /// taken longer than this must also have averaged 1 KiB a second
    /// (0: no limit). Not on gRPC.
    #[arg(long, env = "CHLOEDB_BODY_TIMEOUT_SECS", default_value_t = 30)]
    body_timeout_secs: u64,
    /// The longest a write may wait for the client to read its answer, in
    /// seconds, before its connection is closed (0: no limit).
    #[arg(long, env = "CHLOEDB_WRITE_TIMEOUT_SECS", default_value_t = 60)]
    write_timeout_secs: u64,
    /// Do not count how often each document is retrieved and each key
    /// asked for (the index inspector's reads; counted in memory by
    /// default, cheaply: see docs/chloedb/server.md).
    #[arg(long)]
    no_read_stats: bool,
    /// The cluster's name, as the command center shows it (default: the
    /// StrataCluster, namespace/name, that runs a coordinator).
    #[arg(long, env = "CHLOEDB_CLUSTER_NAME")]
    cluster_name: Option<String>,
    /// The cluster's environment ("production", "staging"...): the
    /// command center treats one starting with "prod" as production, and
    /// none too, except opened on this machine (localhost), where it is a
    /// sandbox whose writes need no second ask.
    #[arg(long, env = "CHLOEDB_ENVIRONMENT")]
    environment: Option<String>,
    /// Where people reach this server's command center
    /// (https://chloedb.example.com), for links in alerts' webhooks.
    #[arg(long, env = "CHLOEDB_PUBLIC_URL")]
    public_url: Option<String>,
    /// The address other coordinators reach this coordinator's HTTP API at
    /// (http://10.0.0.5:7700); it names itself so to its shards, which tell
    /// the others. Default: $CHLOEDB_POD_IP and the --listen port, else the
    /// --listen address.
    #[arg(long, env = "CHLOEDB_PEER_URL")]
    peer_url: Option<String>,
    /// A coordinator's Contain rules' file, which they are kept in across a
    /// restart. Default: contain.json beside --alerts' file (in memory
    /// without either).
    #[arg(long, env = "CHLOEDB_CONTAIN")]
    contain: Option<PathBuf>,
    /// The share of queries that may fail on the server's side before
    /// fleet health says so (GET /v1/health/fleet).
    #[arg(long, default_value_t = 0.01)]
    slo_error_ratio: f64,
    /// The p99 latency of queries, in ms, above which fleet health says so.
    #[arg(long, default_value_t = 250.0)]
    slo_p99_ms: f64,
    /// Proxies (addresses or networks, comma-separated: 10.0.0.0/8) whose
    /// X-Forwarded-For names the client in the audit log; others' is
    /// ignored.
    #[arg(long, env = "CHLOEDB_TRUST_PROXY", value_delimiter = ',')]
    trust_proxy: Vec<String>,
    /// An app the command center shows as a view, as name=url
    /// (crawler=http://127.0.0.1:7800; repeatable, or comma-separated in
    /// $CHLOEDB_APPS): the server forwards /v1/apps/<name>/... to it.
    #[arg(long = "app", env = "CHLOEDB_APPS", value_delimiter = ',')]
    apps: Vec<String>,
    /// The token the server sends its apps (Authorization: Bearer), to
    /// each that has no token of its own ($CHLOEDB_APP_TOKEN_<NAME>, or a
    /// file of --app-token-dir).
    #[arg(long, env = "CHLOEDB_APP_TOKEN", hide_env_values = true)]
    app_token: Option<String>,
    /// Experimental: serve tenants, databases kept whole in an object
    /// store (an `s3://bucket/prefix/` URL or a directory), under
    /// /v1/admin/tenants. Their layout may change without a migration.
    #[arg(long, env = "CHLOEDB_TENANTS")]
    tenants: Option<String>,
    /// Where this server keeps its copies of the tenants it has open (a
    /// cache; default: a directory under the system's temporary one).
    #[arg(long, env = "CHLOEDB_TENANT_CACHE")]
    tenant_cache: Option<PathBuf>,
    /// Close a tenant no request has named for this many seconds (its
    /// copy in the cache is kept).
    #[arg(long, default_value_t = 300.0)]
    tenant_idle_s: f64,
    /// Keep at most this many tenants open: beyond it, the least recently
    /// used are closed.
    #[arg(long, default_value_t = 1000)]
    tenant_open_max: usize,
    /// The apps this server starts itself, as child processes on loopback
    /// ports, each with a token and a scoped key of its own: auto (those of
    /// crawler, sheets and chloectx whose programs are installed beside
    /// chloedb-server, on a server of its own or a coordinator), off, or
    /// names (crawler,sheets,chloectx; looked for on the PATH too).
    #[arg(long, env = "CHLOEDB_START_APPS", default_value = "auto")]
    start_apps: String,
    /// Whether this server runs its connectors' pulls on their schedules
    /// (https://chloetech-ai.github.io/chloedb/site/docs/connectors.html): auto
    /// (on a server of its own or a coordinator; never on a replica or a
    /// shard) or off. Pushes are taken either way.
    #[arg(long, env = "CHLOEDB_CONNECTORS", default_value = "auto")]
    connectors: String,
    /// Where the connectors' secrets are kept (default:
    /// DB.connectors.json beside --db, readable by this user only; a
    /// coordinator without one keeps them in memory, so give it one, or
    /// name secrets as env:VARIABLE).
    #[arg(long)]
    connector_secrets: Option<PathBuf>,
    /// The prefix of the environment variables a connector's secret may
    /// name as env:VARIABLE (default CHLOEDB_CONNECTOR_SECRET_): the
    /// others, this server's own tokens and keys among them, are never
    /// read for a connector.
    #[arg(long, env = "CHLOEDB_CONNECTOR_ENV_PREFIX")]
    connector_env_prefix: Option<String>,
    /// A folder that holds each app's own token in a file of the app's
    /// name, read as it is sent (so a token written there later, as a
    /// Kubernetes Secret mounted there is, needs no restart).
    #[arg(long, env = "CHLOEDB_APP_TOKEN_DIR")]
    app_token_dir: Option<PathBuf>,
}

#[derive(clap::Subcommand)]
enum TenantsCommand {
    /// Probe a store before tenants are kept in it: create an object
    /// twice, replace it at a stale version, read after each write. Exits
    /// with an error saying which the store got wrong.
    CheckStore {
        /// An `s3://bucket/prefix/` URL (configured by environment, as
        /// `import` reads S3) or a directory.
        url: String,
    },
    /// The ids of the tenants a store holds.
    List {
        /// The store, as for check-store.
        url: String,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Experimental: look at an object store that holds tenants
    /// (`--tenants`), without a server.
    Tenants {
        #[command(subcommand)]
        what: TenantsCommand,
    },
    /// Copy a database written by a version of ChloeDB from before field
    /// ids, which no longer opens, into this version's format in its place;
    /// the old one is kept as <db>.before-field-ids. The server must not
    /// have it open.
    Upgrade {
        /// The database: a file, or a directory in the directory layout.
        db: PathBuf,
    },
    /// Download an embedding model from Hugging Face into the models directory.
    FetchModel {
        /// A known name (potion-retrieval-32M, potion-base-8M, their int8
        /// copies potion-retrieval-32M-int8 and potion-base-8M-int8, bge-small-en-v1.5,
        /// snowflake-arctic-embed-m-v1.5, siglip-base-patch16-224,
        /// blip-image-captioning-base, whisper-small, clap-htsat-unfused) or a
        /// repository, owner/name.
        name: String,
        /// Where to put it (default: --models, $CHLOEDB_MODELS, or
        /// ~/.local/share/chloedb/models).
        #[arg(long, env = "CHLOEDB_MODELS")]
        models: Option<PathBuf>,
    },
    /// Check this machine for the guides: this version against the latest
    /// release, the Python package (ChloeDB or PyPI's placeholder), the
    /// models folder, port 7700, and glibc against the prebuilt programs.
    /// Changes nothing; exits 1 when a check fails.
    Doctor {
        /// The models folder to look in (default: $CHLOEDB_MODELS, or
        /// ~/.local/share/chloedb/models).
        #[arg(long, env = "CHLOEDB_MODELS")]
        models: Option<PathBuf>,
        /// The port the guides' servers listen on.
        #[arg(long, default_value_t = 7700)]
        port: u16,
        /// Do not ask GitHub for the latest release.
        #[arg(long)]
        offline: bool,
        /// The Python interpreter whose chloedb package to check.
        #[arg(long, default_value = "python3")]
        python: String,
    },
    /// Write a static (Model2Vec) model with its table in int8 (or f16):
    /// 4x (2x) smaller, ranking within 0.001 nDCG@10 of float32 on BEIR.
    /// The copy is a model of its own name: give a field that name.
    QuantizeModel {
        /// The model's directory (with model.safetensors and tokenizer.json),
        /// or its name under --models.
        model: String,
        /// The directory to write (default: the model's, with -int8 or -f16
        /// after its name, beside it).
        #[arg(long)]
        to: Option<PathBuf>,
        /// I8 (default) or F16.
        #[arg(long, default_value = "I8")]
        to_type: String,
        #[arg(long, env = "CHLOEDB_MODELS")]
        models: Option<PathBuf>,
    },
    /// Serve the Model Context Protocol over stdin and stdout for an agent
    /// such as Claude Code (`claude mcp add chloedb -- chloedb-server mcp
    /// --url http://host:7700`): each message goes to the server's
    /// /v1/mcp with the key, whose scope decides what the agent may change.
    Mcp {
        /// The server's HTTP URL (a coordinator, or a single server).
        #[arg(long, env = "CHLOEDB_URL", default_value = "http://127.0.0.1:7700")]
        url: String,
        /// The API key (or the server's token) to send as a bearer.
        #[arg(long, env = "CHLOEDB_KEY", hide_env_values = true)]
        key: Option<String>,
        /// A CA certificate (PEM) to trust for an https URL, beside the
        /// system's.
        #[arg(long)]
        ca: Option<PathBuf>,
    },
    /// The API from a shell, for scripts and agents without MCP: one
    /// request, its answer's JSON on stdout, and an exit code by what
    /// happened (0 done, 1 failed, 2 refused for the key, 3 stale: look
    /// again, 4 retry later, 5 not reached).
    Admin {
        /// The server's HTTP URL (a coordinator, or a single server).
        #[arg(long, env = "CHLOEDB_URL", default_value = "http://127.0.0.1:7700")]
        url: String,
        /// The API key (or the server's token) to send as a bearer.
        #[arg(long, env = "CHLOEDB_KEY", hide_env_values = true)]
        key: Option<String>,
        /// A CA certificate (PEM) to trust for an https URL.
        #[arg(long)]
        ca: Option<PathBuf>,
        /// Why: sent as x-chloedb-reason, kept in the audit log (an
        /// agent's change needs one).
        #[arg(long, env = "CHLOEDB_REASON")]
        reason: Option<String>,
        /// Print the answer on one line.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        what: AdminCmd,
    },
    /// Make a running replica the primary of a new term (POST
    /// /v1/admin/promote), once a majority of its shard's copies vote for
    /// it: a primary that serves steps down. It refuses while a copy holds
    /// transactions it does not, or when it is not elected, unless --force.
    Promote {
        /// The replica's HTTP URL, as http://host:7700.
        url: String,
        /// The copies it is to ship to (comma-separated gRPC URLs), which
        /// vote: the shard's other replicas, and the old primary, which is
        /// sent a copy once it starts again with --replica. None: the
        /// servers its cluster map names for its shard, or without a map
        /// the copies it was started with (its --replicas). Promotion calls
        /// each with the cluster's token, so with an API key only those may
        /// be named; with the token, any.
        #[arg(long, value_delimiter = ',')]
        replicas: Vec<String>,
        /// The copies that hold a write before it is answered, as
        /// --write-acks: all, majority or primary.
        #[arg(long, default_value = "all")]
        write_acks: String,
        /// Promote it with no votes, though a copy holds transactions it
        /// does not, which a copy of the new primary then drops: only when
        /// a majority of the shard's copies is lost for good.
        #[arg(long)]
        force: bool,
        /// The replica's --token.
        #[arg(long, env = "CHLOEDB_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Follow another cluster: copy its rows, tables and rank profiles into
    /// this one, then keep applying its writes, a moment behind, until
    /// stopped. While it follows, this cluster takes no writes but this
    /// process's (POST /v1/admin/follow/stop ends that). It reads each of
    /// the source's shards' feeds (shard protocol 14; 15 to catch up once
    /// it falls behind a shard's log) and writes through this cluster's
    /// coordinator; GET /v1/admin/follow there shows how far it is.
    /// Started again, it goes on from where it was.
    Follow {
        /// The source cluster's HTTP URL: its coordinator, or a server of
        /// its own, as https://east.example:7700.
        #[arg(long)]
        source: String,
        /// A key of the source of the feed scope (or above).
        #[arg(long, env = "CHLOEDB_FOLLOW_KEY", hide_env_values = true)]
        source_key: Option<String>,
        /// A CA certificate (PEM) to check the source's TLS certificate by.
        #[arg(long)]
        source_ca: Option<PathBuf>,
        /// This cluster's HTTP URL: its coordinator, or a server of its own.
        #[arg(long)]
        target: String,
        /// This cluster's token (its --cluster-token, or the --token of a
        /// server of its own).
        #[arg(long, env = "CHLOEDB_CLUSTER_TOKEN", hide_env_values = true)]
        target_token: Option<String>,
        /// A CA certificate (PEM) to check the target's TLS certificate by.
        #[arg(long)]
        target_ca: Option<PathBuf>,
        /// The name this cluster knows the source by (letters, digits, '.',
        /// '_' and '-'), which its marks and refusals name.
        #[arg(long)]
        name: String,
        /// The most operations of one write.
        #[arg(long, default_value_t = chloedb_server::cores::follow::BATCH)]
        batch: usize,
        /// Copy the source again from the start, whatever this cluster
        /// kept: after a failover of the source lost transactions it
        /// applied, or to follow again a cluster this one was promoted
        /// from. (One that falls behind the source's log catches up by
        /// itself.)
        #[arg(long)]
        copy: bool,
        /// This cluster is the one the source was promoted from (a
        /// switch-over): follow the source back from the promotion's
        /// numbers, without a copy. Refused if this cluster took writes
        /// the source never applied (then follow with --copy).
        #[arg(long, conflicts_with = "copy")]
        reverse: bool,
        /// For a process started again by itself (the operator's follow
        /// Deployment): when it was asked for, in ms since the epoch.
        /// --copy is then done once (a copy begun since is resumed), and
        /// once this cluster stopped following the source or was promoted
        /// from it since, the process does not follow again: it waits,
        /// saying so in GET /v1/admin/follow, until it is stopped.
        #[arg(long, value_name = "MS")]
        once: Option<u64>,
    },
    /// Follow a Postgres database by logical replication: copy the tables
    /// of a publication into ChloeDB (making the tables it lacks), then
    /// keep applying their inserts, updates, deletes, truncates and new
    /// columns, a moment behind, until stopped. Postgres needs wal_level =
    /// logical and a publication (CREATE PUBLICATION strata FOR TABLE
    /// ...); the user signs in with the REPLICATION attribute. What it
    /// applied is kept in ChloeDB's _sources (postgres/NAME/...), so that
    /// started again it goes on from there and applies each change once.
    FollowPostgres {
        /// The database, as postgres://user:password@host:5432/database
        /// (?sslmode=require and the like; PGPASSWORD gives a password the
        /// URL leaves out).
        #[arg(long, env = "CHLOEDB_POSTGRES_URL", hide_env_values = true)]
        source: String,
        /// A CA certificate (PEM) to check the database's TLS certificate by.
        #[arg(long)]
        source_ca: Option<PathBuf>,
        /// The publication whose tables it follows.
        #[arg(long, default_value = "strata")]
        publication: String,
        /// The replication slot it reads and makes (strata_NAME by default).
        #[arg(long)]
        slot: Option<String>,
        /// ChloeDB's HTTP URL: a server, or a cluster's coordinator.
        #[arg(long)]
        target: String,
        /// ChloeDB's token (a server's --token, or a cluster's --cluster-token).
        #[arg(long, env = "CHLOEDB_CLUSTER_TOKEN", hide_env_values = true)]
        target_token: Option<String>,
        /// A CA certificate (PEM) to check the target's TLS certificate by.
        #[arg(long)]
        target_ca: Option<PathBuf>,
        /// The name ChloeDB knows the database by (letters, digits, '.', '_'
        /// and '-'), which its marks in _sources name.
        #[arg(long)]
        name: String,
        /// The most operations of one write while copying, and of the
        /// transactions written together.
        #[arg(long, default_value_t = chloedb_server::postgres::BATCH)]
        batch: usize,
        /// Copy the tables again from the start (with a new slot), whatever
        /// ChloeDB kept.
        #[arg(long)]
        copy: bool,
        /// How many tables are copied at once.
        #[arg(long, default_value_t = 4)]
        copy_workers: usize,
        /// Drop the replication slot and exit: stop following for good, so
        /// that Postgres no longer keeps WAL for the follower.
        #[arg(long)]
        drop_slot: bool,
    },
    /// Switch clients over to a cluster that follows another: hold the
    /// source's writes (POST /v1/admin/hold-writes there), wait until
    /// the follower applied them all, and promote it
    /// (POST /v1/admin/follow/promote), which then takes its own writes
    /// having lost none. The old source can then follow it back
    /// (chloedb-server follow --reverse). Prints the promotion.
    SwitchOver {
        /// The follower's HTTP URL: its coordinator, or a server of its own.
        #[arg(long)]
        target: String,
        /// The follower's cluster token.
        #[arg(long, env = "CHLOEDB_CLUSTER_TOKEN", hide_env_values = true)]
        target_token: Option<String>,
        /// A CA certificate (PEM) to check the follower's TLS certificate by.
        #[arg(long)]
        target_ca: Option<PathBuf>,
        /// The name the follower knows the source by.
        #[arg(long)]
        name: String,
        /// The source's HTTP URL; by default the one its follow process
        /// names.
        #[arg(long)]
        source: Option<String>,
        /// An admin key of the source (or its token), which holds its writes.
        #[arg(long, env = "CHLOEDB_SOURCE_KEY", hide_env_values = true)]
        source_key: Option<String>,
        /// A CA certificate (PEM) to check the source's TLS certificate by.
        #[arg(long)]
        source_ca: Option<PathBuf>,
        /// Promote though the source's writes cannot be held (it is gone):
        /// the writes the follower did not apply are lost.
        #[arg(long)]
        unplanned: bool,
        /// How long to wait for the follower to apply the source's writes,
        /// in seconds.
        #[arg(long, default_value_t = 120)]
        wait_s: u64,
    },
    /// Run the Kubernetes operator: it makes and changes the ChloeDB clusters
    /// that StrataCluster resources describe (see deploy/README.md). In a
    /// pod it talks to the API server as its service account.
    Operator {
        /// Only the clusters of this namespace (default: every namespace).
        #[arg(long)]
        namespace: Option<String>,
        /// How often each cluster is looked at, in milliseconds.
        #[arg(long, default_value_t = 3000)]
        every_ms: u64,
        /// Do not install or update the StrataCluster resource definition
        /// when starting (it needs to be there already).
        #[arg(long)]
        no_crd: bool,
        /// Print the resource definition (JSON, for kubectl apply -f) and
        /// exit.
        #[arg(long)]
        print_crd: bool,
        /// The API server, outside a pod (as https://host:6443).
        #[arg(long)]
        kube_api: Option<String>,
        /// A bearer token for --kube-api.
        #[arg(long, env = "CHLOEDB_KUBE_TOKEN", hide_env_values = true)]
        kube_token: Option<String>,
        /// The CA of --kube-api, PEM.
        #[arg(long)]
        kube_ca: Option<PathBuf>,
    },
    /// Load rows from JSON Lines, JSON, CSV or TSV files, the tables of
    /// SQLite databases, and Parquet files or directories of them (as Spark
    /// writes), into a database. A file may be an s3:// URL: an object, or
    /// every object under a prefix (configured by AWS_* variables). A
    /// database that does not exist is created with --schema, or from the
    /// first SQLite or Parquet file. A SQLite table goes into the table of
    /// its name, and Parquet rows into --table (default: the file's name),
    /// created from the files' types and values when missing. A server must
    /// not have the database open (import through `POST /v1/import`
    /// instead).
    Import {
        /// The database.
        #[arg(long)]
        db: PathBuf,
        /// Schema of a new database: a JSON file, or the JSON itself (a
        /// value that starts with `{`). With SQLite files, schemas
        /// for the tables it creates, in place of the ones made from
        /// SQLite's: one, an array, or the output of --plan, edited. With
        /// Parquet files, the schema of the table they create (the output
        /// of --plan, edited).
        #[arg(long)]
        schema: Option<PathBuf>,
        /// The table the rows go to (default: the first; for Parquet, the
        /// table an export kept in the file, else the file's name). A
        /// SQLite file's tables go to the tables of their names.
        #[arg(long)]
        table: Option<String>,
        /// The files' format: jsonl, json, csv, tsv, sqlite or parquet
        /// (default: SQLite or Parquet when the file or directory is one,
        /// else from each file's extension).
        #[arg(long)]
        format: Option<String>,
        /// SQLite: import only these tables (comma-separated).
        #[arg(long, value_delimiter = ',')]
        tables: Vec<String>,
        /// Parquet: the column that is the key of a table the import
        /// creates, in place of one named id; "" for no key.
        #[arg(long)]
        key: Option<String>,
        /// SQLite and Parquet: keep binary columns as base64 text, rather
        /// than leave them out.
        #[arg(long)]
        blobs_base64: bool,
        /// SQLite and Parquet: print what the import would do as JSON (each
        /// table's schema, its columns and what is left out), and write
        /// nothing. Edit it and give it back with --schema.
        #[arg(long)]
        plan: bool,
        /// Where embedded fields' models are (see `fetch-model`).
        #[arg(long, env = "CHLOEDB_MODELS")]
        models: Option<PathBuf>,
        /// Create a new database as a directory of segment files.
        #[arg(long)]
        directory: bool,
        /// The files (or s3:// URLs), loaded in order.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Write a database's tables, from one snapshot, to a new SQLite file
    /// (the rows, keys, references, NOT NULL and DEFAULT, a B-tree index on
    /// each indexed KEYWORD, INT, FLOAT, BOOL and TIMESTAMP field, an FTS5
    /// index over each table's TEXT fields, and the ChloeDB schemas, which
    /// `import` restores), or to Parquet: a `.parquet` file for one table,
    /// else a directory of `<table>.parquet` files, each keeping its ChloeDB
    /// schema. OUT may be an s3:// URL. A server must not have the database
    /// open.
    Export {
        /// The database.
        #[arg(long)]
        db: PathBuf,
        /// Only these tables (comma-separated).
        #[arg(long, value_delimiter = ',')]
        tables: Vec<String>,
        /// sqlite or parquet (default: Parquet when OUT ends in .parquet
        /// or /, else SQLite).
        #[arg(long)]
        format: Option<String>,
        /// SQLite: no FTS5 indexes.
        #[arg(long)]
        no_fts: bool,
        /// SQLite: no B-tree indexes.
        #[arg(long)]
        no_indexes: bool,
        /// Leave out the ChloeDB schemas: SQLite's table `_strata_tables`,
        /// each Parquet file's `strata.schema` metadata.
        #[arg(long)]
        no_schemas: bool,
        /// The file or directory (or s3:// URL) to write; it must not
        /// exist.
        out: String,
    },
}

fn main() {
    // Settings may be named CHLOEDB_* (the name since the rename) or STRATA_*:
    // clap and the code read the old names, so the new ones are copied over
    // before the runtime starts any thread.
    chloedb::env::adopt_old_names();
    // As #[tokio::main] builds it, with a larger stack for each worker and
    // blocking thread, where queries are planned and run: the statements
    // the parser takes fit in 2 MiB, and this leaves room for more.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(chloedb::query::parse::QUERY_STACK)
        .build()
        .expect("start the tokio runtime")
        .block_on(run());
}

/// The server's options by topic, for `--help`: every option of [`Args`]
/// is under one (a test says so), so that the few a first run needs are
/// not lost among the rest. `-h` lists the first topic's only.
const HELP_TOPICS: &[(&str, &[&str])] = &[
    ("Start", &[
        "db", "schema", "demo", "models", "listen", "grpc", "token", "no_token", "log",
        "start_apps"
    ]),
    ("Database", &[
        "demo_shard", "copy_from", "no_backfill", "no_fetch_models", "directory",
        "wal_drop_damaged", "recover", "verify_checksums", "seq_no", "idempotency_ttl_s",
        "scrub_every_s", "scrub_mib_s", "synonyms", "backup_dir", "slices", "slices_rebuild_s",
        "no_read_stats"
    ]),
    ("Merges and storage tiers", &[
        "merge_threads", "merge_memory_mb", "merge_write_mb_per_sec", "merge_nice", "cold_store",
        "cold_cache_gb", "cold_keep_local_gb", "cold_min_segment_mb", "cold_cache_dir"
    ]),
    ("Replication", &[
        "replicas", "replica", "promote", "write_acks", "ship_log_mb", "verify_every_s",
        "failover_after_ms"
    ]),
    ("Cluster: a coordinator and its shards", &[
        "shards", "hedge_ms", "shard_timeout_ms", "route", "hedge_adaptive", "prefer_caught_up",
        "partial", "stats_cache_terms", "write_every_copy", "kube_cluster", "coordinator_listen",
        "local_shard", "cluster_token", "shard_ca", "slot_map", "peer_url", "cluster_name",
        "node_name", "environment", "public_url", "image_repository"
    ]),
    ("Security", &[
        "allow_host", "tls_cert", "tls_key", "keys", "audit_log", "trust_proxy", "redact_params"
    ]),
    ("Limits and timeouts", &[
        "timeout_ms", "max_timeout_ms", "max_concurrent", "admission_wait_ms", "max_result_window",
        "max_scroll_rows", "max_groups", "max_rerank", "llm_rows", "llm_per_minute",
        "model_calls_per_key_day", "query_memory_mb", "filter_cache_mb", "answer_cache_mb",
        "max_statement_bytes", "max_statement_elements", "max_queued_writes", "max_body_mb", "max_connections", "max_connections_per_client",
        "header_timeout_secs", "idle_timeout_secs", "body_timeout_secs", "write_timeout_secs",
        "drain_ms", "stop_grace_ms"
    ]),
    ("Agent, models and media", &[
        "model", "api_base", "media_fetch", "media_max_bytes", "media_store", "media_timeout_s"
    ]),
    ("Logs, alerts and health", &[
        "log_format", "log_store", "log_retain", "log_store_mb", "log_queries", "slow_ms",
        "otlp_retain", "alerts", "alert_webhook", "contain", "slo_error_ratio", "slo_p99_ms"
    ]),
    ("Apps, connectors, tenants and clouds", &[
        "apps", "app_token", "app_token_dir", "connectors", "connector_secrets", "connector_env_prefix", "tenants",
        "tenant_cache", "tenant_idle_s", "tenant_open_max", "opensearch_listen",
        "opensearch_version", "aws_file", "digitalocean_file", "cloud_file", "hub_file",
        "hub_device"
    ]),
];

/// [`Args`]' command, its options under [`HELP_TOPICS`]' headings.
fn args_command() -> clap::Command {
    use clap::CommandFactory;
    let mut cmd = Args::command();
    for (i, (topic, ids)) in HELP_TOPICS.iter().enumerate() {
        for id in *ids {
            cmd = cmd.mut_arg(*id, |a| a.help_heading(*topic).hide_short_help(i > 0));
        }
    }
    cmd.after_help("These are the options of a first run: --help lists all of them, by topic. Each command (import, admin, doctor, ...) has its own --help.")
        .after_long_help("Each command (import, admin, doctor, ...) has its own --help.")
}

async fn run() {
    let a = <Args as clap::FromArgMatches>::from_arg_matches(&args_command().get_matches()).unwrap_or_else(|e| e.exit());
    chloedb::query::llm::set_limits(chloedb::query::llm::Limits { rows: a.llm_rows, per_minute: (a.llm_per_minute > 0).then_some(a.llm_per_minute) });
    let format = obs::LogFormat::parse(&a.log_format).unwrap_or_else(|| {
        eprintln!("chloedb-server: --log-format is text or json, not {:?}", a.log_format);
        std::process::exit(1)
    });
    if let Err(e) = obs::init_logs(&a.log, format) {
        eprintln!("chloedb-server: {e}");
        std::process::exit(1)
    }
    // A server's lines go to its log store from the first (it opens later).
    if a.command.is_none() && a.log_store.as_deref() != Some("off") {
        obs::sink().start();
    }
    match &a.command {
        Some(Command::Tenants { what }) => {
            let (TenantsCommand::CheckStore { url } | TenantsCommand::List { url }) = what;
            let store = chloedb::storage::open_store(url).unwrap_or_else(|e| fail(&format!("{url}: {e}")));
            match what {
                // Whether the store refuses a write whose condition fails:
                // tenants are not safe in one that does not.
                TenantsCommand::CheckStore { .. } => match chloedb::tenant::check_store(&*store) {
                    Ok(()) => out!("{}: creates only what is absent, replaces only the version named, and reads its last write: tenants can be kept here", store.describe()),
                    Err(e) => fail(&e.to_string()),
                },
                TenantsCommand::List { .. } => match chloedb::tenant::list(&*store) {
                    Ok(ids) => ids.iter().for_each(|id| out!("{id}")),
                    Err(e) => fail(&e.to_string()),
                },
            }
            return;
        }
        Some(Command::Upgrade { db }) => {
            match chloedb::upgrade::upgrade(db) {
                Ok(Some(u)) => out!("copied {} rows of {} tables of {} into this version's format; the old database is {}", u.rows, u.tables, db.display(), u.old.display()),
                Ok(None) => out!("{} needs no upgrade", db.display()),
                Err(e) => fail(&format!("upgrade {}: {e}", db.display())),
            }
            return;
        }
        Some(Command::FetchModel { name, models }) => {
            let dir = models.clone().unwrap_or_else(chloedb::embed::models_dir);
            match server::fetch_model(name, &dir).await {
                Ok(path) => tracing::info!(model = name, dir = %path.display(), "fetched the model"),
                Err(e) => fail(&e),
            }
            return;
        }
        Some(Command::Doctor { models, port, offline, python }) => {
            let dir = models.clone().unwrap_or_else(chloedb::embed::models_dir);
            let checks = chloedb_server::doctor::run(&chloedb_server::doctor::Options { models: &dir, port: *port, online: !offline, python }).await;
            for c in &checks {
                out!("{}", c.line());
            }
            if checks.iter().any(|c| c.level == chloedb_server::doctor::Level::Fail) {
                std::process::exit(1);
            }
            return;
        }
        Some(Command::QuantizeModel { model, to, to_type, models }) => {
            let from = match std::path::Path::new(model).is_dir() {
                true => PathBuf::from(model),
                false => models.clone().unwrap_or_else(chloedb::embed::models_dir).join(model),
            };
            let suffix = if to_type.eq_ignore_ascii_case("f16") { "f16" } else { "int8" };
            let to = to.clone().unwrap_or_else(|| from.with_file_name(format!("{}-{suffix}", from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())));
            match chloedb::embed::quantize_static(&from, &to, to_type) {
                Ok(q) => out!("{}: {} rows of {} dimensions, {} MB (was {} MB)", to.display(), q.rows, q.dim, q.bytes_after >> 20, q.bytes_before >> 20),
                Err(e) => fail(&e.to_string()),
            }
            return;
        }
        Some(Command::Mcp { url, key, ca }) => {
            let ca = match ca.as_ref().map(std::fs::read).transpose() {
                Ok(c) => c,
                Err(e) => fail(&format!("--ca: {e}")),
            };
            if let Err(e) = chloedb_server::mcp::stdio(url, key.as_deref(), ca).await {
                fail(&e);
            }
            return;
        }
        Some(Command::Admin { url, key, ca, reason, json, what }) => {
            use chloedb_server::admin_cli::{self as cli, Call};
            let ca = match ca.as_ref().map(std::fs::read).transpose() {
                Ok(c) => c,
                Err(e) => fail(&format!("--ca: {e}")),
            };
            let q = |base: &str, k: &str, v: &Option<String>| match v {
                Some(v) => format!("{base}?{k}={}", chloedb_server::mcp::encode(v)),
                None => base.to_string(),
            };
            let call = match what {
                AdminCmd::Whoami => Ok(Call::get("/v1/whoami")),
                AdminCmd::Health => Ok(Call::get("/v1/health/fleet")),
                AdminCmd::Catalog { q: words } => Ok(Call::get(q("/v1/catalog", "q", words))),
                AdminCmd::Jobs => Ok(Call::get("/v1/admin/jobs")),
                AdminCmd::Audit { limit } => Ok(Call::get(format!("/v1/admin/audit?limit={limit}"))),
                AdminCmd::Changes { id: None, state } => Ok(Call::get(q("/v1/admin/changes", "state", state))),
                AdminCmd::Changes { id: Some(id), .. } => Ok(Call::get(format!("/v1/admin/changes/{}", chloedb_server::mcp::encode(id)))),
                AdminCmd::Runbook { id } => Ok(Call::get(format!("/v1/runbooks?for={}", chloedb_server::mcp::encode(id)))),
                AdminCmd::Call { method, path, body } => cli::call_of(method, path, body.as_deref()),
                AdminCmd::Preview { method, path, body } => cli::call_of(method, path, body.as_deref()).map(|c| cli::previewed(&c)),
                AdminCmd::Propose { method, path, body, what } => match reason.as_deref() {
                    Some(r) if !r.trim().is_empty() => cli::call_of(method, path, body.as_deref()).map(|c| cli::proposed(&c, r, what.as_deref())),
                    _ => Err("a change request says why: --reason".to_string()),
                },
            };
            let call = call.unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1)
            });
            let target = cli::Target { url: url.clone(), key: key.clone(), ca, reason: reason.clone() };
            match cli::send(&target, &call).await {
                Ok((status, answer)) => {
                    let text = if *json { answer.to_string() } else { serde_json::to_string_pretty(&answer).unwrap_or_default() };
                    out!("{text}");
                    std::process::exit(cli::exit_code(status, &answer));
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(5);
                }
            }
        }
        Some(Command::Promote { url, replicas, write_acks, force, token }) => {
            match server::promote(url, replicas, write_acks, *force, token.as_deref()).await {
                Ok(j) => out!("{}", serde_json::to_string_pretty(&j).unwrap_or_default()),
                Err(e) => fail(&e),
            }
            return;
        }
        Some(Command::Follow { source, source_key, source_ca, target, target_token, target_ca, name, batch, copy, reverse, once }) => {
            let read = |p: &Option<PathBuf>, what: &str| p.as_ref().map(|p| std::fs::read(p).unwrap_or_else(|e| fail(&format!("{what} {}: {e}", p.display()))));
            let o = chloedb_server::follow::Options {
                source: source.clone(),
                source_key: source_key.clone(),
                source_ca: read(source_ca, "--source-ca"),
                target: target.clone(),
                target_token: target_token.clone(),
                target_ca: read(target_ca, "--target-ca"),
                name: name.clone(),
                batch: *batch,
                copy: *copy,
                reverse: *reverse,
                once: *once,
            };
            if let Err(e) = chloedb_server::follow::run(o).await {
                fail(&format!("follow {name}: {e}"));
            }
            return;
        }
        Some(Command::FollowPostgres { source, source_ca, publication, slot, target, target_token, target_ca, name, batch, copy, copy_workers, drop_slot }) => {
            let read = |p: &Option<PathBuf>, what: &str| p.as_ref().map(|p| std::fs::read(p).unwrap_or_else(|e| fail(&format!("{what} {}: {e}", p.display()))));
            let o = chloedb_server::postgres::Options {
                source: source.clone(),
                source_ca: read(source_ca, "--source-ca"),
                publication: publication.clone(),
                slot: slot.clone(),
                target: target.clone(),
                target_token: target_token.clone(),
                target_ca: read(target_ca, "--target-ca"),
                name: name.clone(),
                batch: (*batch).max(1),
                copy: *copy,
                copy_workers: *copy_workers,
                crash_after: None,
            };
            if *drop_slot {
                match chloedb_server::postgres::drop_slot(o).await {
                    Ok(j) => out!("{}", serde_json::to_string_pretty(&j).unwrap_or_default()),
                    Err(e) => fail(&format!("follow-postgres {name}: {e}")),
                }
                return;
            }
            if let Err(e) = chloedb_server::postgres::run(o).await {
                fail(&format!("follow-postgres {name}: {e}"));
            }
            return;
        }
        Some(Command::SwitchOver { target, target_token, target_ca, name, source, source_key, source_ca, unplanned, wait_s }) => {
            let read = |p: &Option<PathBuf>, what: &str| p.as_ref().map(|p| std::fs::read(p).unwrap_or_else(|e| fail(&format!("{what} {}: {e}", p.display()))));
            let o = chloedb_server::follow::SwitchOptions {
                target: target.clone(),
                target_token: target_token.clone(),
                target_ca: read(target_ca, "--target-ca"),
                name: name.clone(),
                source: source.clone(),
                source_key: source_key.clone(),
                source_ca: read(source_ca, "--source-ca"),
                unplanned: *unplanned,
                wait: std::time::Duration::from_secs(*wait_s),
            };
            match chloedb_server::follow::switch_over(o).await {
                Ok(j) => out!("{}", serde_json::to_string_pretty(&j).unwrap_or_default()),
                Err(e) => fail(&format!("switch-over to {target}: {e}")),
            }
            return;
        }
        Some(Command::Operator { namespace, every_ms, no_crd, print_crd, kube_api, kube_token, kube_ca }) => {
            if *print_crd {
                out!("{}", serde_json::to_string_pretty(&chloedb_server::operator::crd()).unwrap_or_default());
                return;
            }
            let ca = kube_ca.as_ref().map(|p| std::fs::read(p).unwrap_or_else(|e| fail(&format!("--kube-ca {}: {e}", p.display()))));
            let o = chloedb_server::operator::kube::Options {
                api: kube_api.clone(),
                token: kube_token.clone(),
                ca,
                namespace: namespace.clone(),
                every: std::time::Duration::from_millis((*every_ms).max(100)),
                install_crd: !no_crd,
            };
            if let Err(e) = chloedb_server::operator::kube::run(o).await {
                fail(&e);
            }
            return;
        }
        Some(Command::Import { db, schema, table, format, models, directory, files, tables, key, blobs_base64, plan }) => {
            let named = |n: &str| format.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(n));
            let files_of = server::FileImport { sqlite: named("sqlite"), parquet: named("parquet"), tables: tables.clone(), key: key.clone(), blobs_base64: *blobs_base64, plan: *plan };
            let format = format
                .as_deref()
                .filter(|_| !files_of.sqlite && !files_of.parquet)
                .map(|f| chloedb::ingest::Format::parse(f).unwrap_or_else(|| fail(&format!("unknown --format {f}: jsonl, json, csv, tsv, sqlite or parquet"))));
            let models = models.clone().unwrap_or_else(chloedb::embed::models_dir);
            if let Err(e) = server::import_files(db, schema.as_deref(), table.as_deref(), &models, format, *directory, files, files_of) {
                fail(&e);
            }
            return;
        }
        Some(Command::Export { db, tables, format, no_fts, no_indexes, no_schemas, out }) => {
            let parquet = match format.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("parquet") => true,
                Some("sqlite") => false,
                Some(f) => fail(&format!("unknown --format {f}: sqlite or parquet")),
                None => out.to_ascii_lowercase().ends_with(".parquet") || out.ends_with('/'),
            };
            let r = if parquet {
                server::export_parquet(db, out, &server::ParquetExport { tables: tables.clone(), meta: !no_schemas })
            } else {
                let opts = chloedb::sqlite::ExportOptions { tables: tables.clone(), fts: !no_fts, indexes: !no_indexes, meta: !no_schemas };
                server::export_sqlite(db, out, &opts)
            };
            if let Err(e) = r {
                fail(&e);
            }
            return;
        }
        None => {}
    }
    let demo_shard = a.demo_shard.as_deref().map(|s| {
        let (i, n) = s.split_once('/').unwrap_or_else(|| fail("--demo-shard is I/N"));
        (i.parse().unwrap_or_else(|_| fail("--demo-shard is I/N")), n.parse().unwrap_or_else(|_| fail("--demo-shard is I/N")))
    });
    let read = |p: &PathBuf| std::fs::read(p).unwrap_or_else(|e| fail(&format!("read {}: {e}", p.display())));
    let tls = match (&a.tls_cert, &a.tls_key) {
        (Some(c), Some(k)) => Some(Tls { cert: read(c), key: read(k) }),
        _ => None,
    };
    if let Some(t) = &tls {
        auth::server_config(t).unwrap_or_else(|e| fail(&e));
    }
    let token = a.token.clone().filter(|t| !t.is_empty()).map(Secret::new);
    let cluster = a.cluster_token.clone().filter(|t| !t.is_empty()).map(Secret::new);
    if cluster.is_some() && cluster == token {
        tracing::warn!("--cluster-token is --token: whoever holds the token can make the calls between servers. Give the cluster a token of its own");
    }
    chloedb_server::idem::set_ttl(std::time::Duration::from_secs(a.idempotency_ttl_s));
    let write_acks = chloedb_server::replication::Acks::parse(&a.write_acks).unwrap_or_else(|| fail(&format!("--write-acks is all, majority or primary, not {:?}", a.write_acks)));
    if a.replica && a.grpc == "off" {
        fail("a replica takes its primary's writes over gRPC: --replica needs --grpc");
    }
    chloedb_server::telemetry::set_redact_params(a.redact_params);
    // The apps it starts itself (crate::app_procs).
    let started_apps = {
        let taken: Vec<String> = chloedb_server::apps::parse(&a.apps).unwrap_or_default().into_iter().map(|x| x.name).collect();
        let role = chloedb_server::app_procs::Role {
            replica: a.replica,
            shard: cluster.is_some() && a.shards.is_empty() && a.replicas.is_empty(),
            beside_coordinator: a.coordinator_listen.is_some(),
            kube: a.kube_cluster.is_some() || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some(),
            tls: tls.is_some(),
        };
        let own_dir = std::env::current_exe().ok().and_then(|e| e.canonicalize().ok()).and_then(|e| e.parent().map(|p| p.to_path_buf()));
        chloedb_server::app_procs::plan(&a.start_apps, &taken, &role, own_dir.as_deref()).unwrap_or_else(|e| fail(&e))
    };
    // The connectors' pulls (crate::connectors): not on a replica or a shard.
    let run_connectors = match a.connectors.as_str() {
        "off" => false,
        "auto" | "on" => !a.replica && !(cluster.is_some() && a.shards.is_empty() && a.replicas.is_empty()),
        other => fail(&format!("--connectors is auto or off, not {other:?}")),
    };
    let o = Options {
        db: a.db,
        schema: a.schema,
        copy_from: a.copy_from,
        demo: a.demo,
        demo_shard,
        directory: a.directory,
        wal_drop_damaged: a.wal_drop_damaged,
        recover: a.recover,
        verify_checksums: a.verify_checksums,
        seq_no: a.seq_no,
        replicas: a.replicas,
        replica: a.replica,
        promote: a.promote,
        write_acks,
        ship_log_mb: a.ship_log_mb,
        verify_every_s: a.verify_every_s,
        failover_after_ms: a.failover_after_ms,
        scrub_every_s: a.scrub_every_s,
        scrub_mib_s: a.scrub_mib_s,
        shards: a.shards,
        kube_cluster: a.kube_cluster.filter(|c| !c.is_empty()),
        coord: chloedb_server::coord::CoordOptions {
            hedge: std::time::Duration::from_millis(a.hedge_ms),
            shard_timeout: (a.shard_timeout_ms > 0).then(|| std::time::Duration::from_millis(a.shard_timeout_ms)),
            partial: a.partial,
            route: chloedb_server::coord::Route::parse(&a.route).unwrap_or_else(|| fail(&format!("--route is round-robin or least-cost, not {:?}", a.route))),
            stats_cache_terms: a.stats_cache_terms,
            write_every_copy: a.write_every_copy,
            hedge_adaptive: a.hedge_adaptive,
            prefer_caught_up: a.prefer_caught_up,
            token: cluster.clone().or_else(|| token.clone()),
            ca: a.shard_ca.as_ref().map(read),
            slots: a.slot_map.as_ref().map(|p| {
                let j: serde_json::Value = serde_json::from_slice(&read(p)).unwrap_or_else(|e| fail(&format!("{}: {e}", p.display())));
                chloedb_server::slots::SlotMap::from_json(&j).unwrap_or_else(|e| fail(&format!("{}: {e}", p.display())))
            }),
            ..Default::default()
        },
        timeout_ms: a.timeout_ms,
        max_timeout_ms: a.max_timeout_ms,
        max_concurrent: a.max_concurrent,
        admission_wait_ms: a.admission_wait_ms,
        max_result_window: a.max_result_window,
        max_scroll_rows: Some(a.max_scroll_rows),
        max_groups: a.max_groups,
        max_rerank: a.max_rerank,
        query_memory_mb: a.query_memory_mb,
        filter_cache_mb: a.filter_cache_mb,
        answer_cache_mb: a.answer_cache_mb,
        max_statement_bytes: Some(a.max_statement_bytes),
        max_statement_elements: Some(a.max_statement_elements),
        max_queued_writes: a.max_queued_writes,
        max_body_mb: a.max_body_mb,
        otlp_retain: a.otlp_retain,
        model: a.model,
        api_key: std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()),
        api_base: a.api_base,
        models: a.models.unwrap_or_else(chloedb::embed::models_dir),
        backfill: !a.no_backfill,
        fetch_models: !a.no_fetch_models,
        synonyms: a.synonyms,
        backup_dir: a.backup_dir,
        slices: a.slices,
        slices_rebuild_s: a.slices_rebuild_s,
        cold: a.cold_store.clone().map(|store| chloedb_server::server::ColdFlags {
            store,
            cache_bytes: (a.cold_cache_gb * (1u64 << 30) as f64) as u64,
            keep_local_bytes: a.cold_keep_local_gb.map(|g| (g * (1u64 << 30) as f64) as u64),
            min_segment_bytes: a.cold_min_segment_mb << 20,
            cache_dir: a.cold_cache_dir.clone(),
        }),
        media_fetch: a.media_fetch,
        // The default needs no resolver of its own: a database's has it.
        media_max_bytes: if a.media_max_bytes == chloedb::media::DEFAULT_MAX_BYTES { 0 } else { a.media_max_bytes },
        media_store: a.media_store,
        media_timeout_s: a.media_timeout_s,
        slow_ms: a.slow_ms,
        keys: a.keys,
        audit_log: a.audit_log,
        aws_file: a.aws_file,
        digitalocean_file: a.digitalocean_file,
        cloud_file: a.cloud_file,
        hub_file: a.hub_file,
        connector_secrets: a.connector_secrets.clone(),
        connector_env_prefix: a.connector_env_prefix.clone(),
        connectors: run_connectors,
        hub_device: a.hub_device,
        contain: a.contain.clone().or_else(|| a.alerts.as_ref().map(|p| p.with_file_name("contain.json"))),
        alerts: a.alerts,
        alert_webhook: a.alert_webhook.filter(|u| !u.is_empty()),
        log_store: a.log_store.as_deref().map(|p| (p != "off").then(|| PathBuf::from(p))),
        node_name: a.node_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| {
            let port = a.listen.rsplit(':').next().unwrap_or("7700");
            format!("{}:{port}", chloedb_server::logstore::hostname())
        }),
        log_retain: std::time::Duration::from_secs(chloedb::schema::Retain::parse_period(&a.log_retain).unwrap_or_else(|e| fail(&format!("--log-retain: {e}"))) as u64),
        log_store_bytes: a.log_store_mb << 20,
        log_queries_per_sec: a.log_queries,
        cluster_name: a.cluster_name.clone(),
        environment: a.environment.clone(),
        public_url: a.public_url.clone(),
        slo_error_ratio: Some(a.slo_error_ratio),
        slo_p99_ms: Some(a.slo_p99_ms),
        trust_proxy: a.trust_proxy.clone(),
        apps: a.apps.clone(),
        tenants: a.tenants.clone(),
        tenant_cache: a.tenant_cache.clone(),
        tenant_idle_s: Some(a.tenant_idle_s),
        tenant_open_max: Some(a.tenant_open_max),
        app_token: a.app_token.clone(),
        started_apps,
        app_token_dir: a.app_token_dir.clone(),
        node_name_given: a.node_name.clone().filter(|n| !n.is_empty()),
        image_repository: a.image_repository.clone().filter(|r| !r.trim().is_empty()),
        merger: {
            let d = chloedb::storage::MergerOptions::default();
            chloedb::storage::MergerOptions {
                threads: a.merge_threads.unwrap_or(d.threads),
                memory_budget: a.merge_memory_mb.map(|m| m << 20),
                write_bytes_per_sec: a.merge_write_mb_per_sec.map(|m| m << 20),
                nice: a.merge_nice.unwrap_or(d.nice),
                ..d
            }
        },
    };
    // --coordinator-listen: a shard server with a coordinator beside it, in
    // this process. The coordinator keeps none of the files a server may
    // be given (the shard does), nor a part in replication.
    let beside = a.coordinator_listen.clone().map(|addr| {
        let local = match (&o.db, a.grpc.as_str(), o.shards.is_empty(), &a.local_shard) {
            (Some(_), g, false, Some(url)) if g != "off" && o.shards.iter().any(|s| s.split('|').any(|u| u == url)) => url.clone(),
            _ => fail("--coordinator-listen serves a coordinator beside this shard server: it needs --db, --grpc, --shards and --local-shard, the URL of --shards that is this server"),
        };
        let co = Options {
            db: None,
            replicas: Vec::new(),
            replica: false,
            promote: false,
            seq_no: false,
            keys: None,
            audit_log: None,
            aws_file: None,
            digitalocean_file: None,
            cloud_file: None,
            hub_file: None,
            connector_secrets: o.connector_secrets.clone().or_else(|| o.db.as_deref().map(chloedb_server::connectors::Connectors::file_of)),
            alerts: None,
            contain: None,
            backup_dir: None,
            slices: None,
            log_store: None,
            ..o.clone()
        };
        (addr, local, co)
    });
    let o = match beside {
        Some(_) => Options { shards: Vec::new(), connectors: false, ..o },
        None => o,
    };
    let coordinator = !o.shards.is_empty();
    server::set_drain(std::time::Duration::from_millis(a.drain_ms));
    let svc = server::service(&o).await.unwrap_or_else(|e| fail(&e));
    svc.reads.set_on(!a.no_read_stats);
    svc.model_budget.set_per_day(a.model_calls_per_key_day);
    if token.is_none() && cluster.is_none() && !svc.keys.is_empty() && !svc.keys.managed() {
        tracing::warn!("this server has no token and no admin key that never expires: once its admin keys expire, only starting it with --token manages it. Make an admin key without expires_days");
    }
    let bind = |addr: &str| {
        let addr = addr.to_string();
        // API keys close a server as a token does.
        let open = token.is_none() && cluster.is_none() && svc.keys.is_empty() && !a.no_token;
        async move {
            let l = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| fail(&format!("listen on {addr}: {e}")));
            let ip = l.local_addr().unwrap_or_else(|e| fail(&format!("listen on {addr}: {e}"))).ip();
            if open && !ip.is_loopback() {
                fail(&format!("{addr} is reachable beyond this machine: set --token (or $CHLOEDB_TOKEN), make an API key, or pass --no-token to serve it open"));
            }
            (l, ip)
        }
    };
    let (http, ip) = bind(&a.listen).await;
    let access = Access { token: token.clone(), cluster: cluster.clone(), hosts: auth::allowed_hosts(ip, &a.allow_host), tls, limits: auth::Limits {
        header_read: std::time::Duration::from_secs(a.header_timeout_secs.max(1)),
        idle: std::time::Duration::from_secs(a.idle_timeout_secs),
        body: std::time::Duration::from_secs(a.body_timeout_secs),
        write: std::time::Duration::from_secs(a.write_timeout_secs),
        max_connections: a.max_connections,
        per_client: a.max_connections_per_client,
        stop_grace: std::time::Duration::from_millis(a.stop_grace_ms),
    } };
    // fleet: a coordinator names itself to its shards, which tell the others.
    if coordinator {
        let at = http.local_addr().unwrap_or_else(|e| fail(&format!("listen on {}: {e}", a.listen)));
        chloedb_server::peers::start(&svc, chloedb_server::peers::own_url(a.peer_url.as_deref(), at, access.tls.is_some()));
    }
    let h = svc.health().await;
    tracing::info!(table = %svc.schema.table, rows = %h["docs"], segments = %h["segments"], version = env!("CARGO_PKG_VERSION"), simd = %chloedb::simd::describe(), "serving");
    let scheme = if access.tls.is_some() { "https" } else { "http" };
    let asks = if access.token.is_some() { " (it asks for the token)" } else { "" };
    tracing::info!("HTTP and UI on {scheme}://{}/{asks}", a.listen);
    if svc.agent.api_key.is_none() {
        tracing::info!("no ANTHROPIC_API_KEY; the agent asks for a key in the UI");
    }
    let mut tasks = Vec::new();
    // A coordinator answers queries but is not itself a shard.
    if a.grpc != "off" && !coordinator {
        let (l, _) = bind(&a.grpc).await;
        tracing::info!("gRPC on {}{}", a.grpc, if access.tls.is_some() { " (TLS)" } else { "" });
        let (svc, access) = (svc.clone(), access.clone());
        tasks.push(tokio::spawn(async move {
            if let Err(e) = server::serve_grpc(svc, l, access).await {
                tracing::error!(error = %e, "gRPC stopped");
            }
        }));
    }
    // The OpenSearch API answers for what this process serves: beside a
    // coordinator (--coordinator-listen), for the cluster, once the
    // coordinator is up.
    let mut opensearch = None;
    if let Some(addr) = &a.opensearch_listen {
        let (l, ip) = bind(addr).await;
        let scheme = if access.tls.is_some() { "https" } else { "http" };
        tracing::info!("OpenSearch API on {scheme}://{addr}/ (version {})", a.opensearch_version);
        opensearch = Some((l, Access { hosts: auth::allowed_hosts(ip, &a.allow_host), ..access.clone() }, a.opensearch_version.clone()));
    }
    if beside.is_none() {
        if let Some((l, os_access, version)) = opensearch.take() {
            tasks.push(tokio::spawn(serve_opensearch(svc.clone(), l, os_access, version)));
        }
    }
    if let Some((addr, url, mut co)) = beside {
        co.coord.local = Some(chloedb_server::coord::LocalShard { url, service: server::grpc_service(svc.clone(), &access) });
        let (l, cip) = bind(&addr).await;
        let access = Access { hosts: auth::allowed_hosts(cip, &a.allow_host), ..access.clone() };
        let (peer, no_read_stats, model_calls) = (a.peer_url.clone(), a.no_read_stats, a.model_calls_per_key_day);
        // Its shards, this one among them, may answer only later: the
        // shard serves meanwhile.
        tasks.push(tokio::spawn(async move {
            let c = server::service(&co).await.unwrap_or_else(|e| fail(&format!("the coordinator: {e}")));
            c.reads.set_on(!no_read_stats);
            c.model_budget.set_per_day(model_calls);
            let at = l.local_addr().unwrap_or_else(|e| fail(&format!("listen on {addr}: {e}")));
            chloedb_server::peers::start(&c, chloedb_server::peers::own_url(peer.as_deref(), at, access.tls.is_some()));
            tracing::info!("coordinator HTTP on {addr}, its own shard in-process");
            if let Some((ol, os_access, version)) = opensearch {
                tokio::spawn(serve_opensearch(c.clone(), ol, os_access, version));
            }
            if let Err(e) = server::serve_http(c, l, access).await {
                fail(&format!("coordinator HTTP server: {e}"));
            }
        }));
    }
    let store = svc.log_store.clone();
    // The apps it starts itself call it back on loopback.
    if !svc.apps.procs.is_empty() {
        let at = http.local_addr().unwrap_or_else(|e| fail(&format!("listen on {}: {e}", a.listen)));
        let host = match at.ip() {
            std::net::IpAddr::V4(ip) if ip.is_unspecified() => std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V6(ip) if ip.is_unspecified() => std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ip => ip,
        };
        svc.apps.start(&format!("http://{}", std::net::SocketAddr::new(host, at.port())), std::sync::Arc::new(chloedb_server::app_procs::ServerKeys { keys: svc.keys.clone() }));
    }
    let apps = svc.clone();
    if let Err(e) = server::serve_http(svc, http, access).await {
        fail(&format!("HTTP server: {e}"));
    }
    apps.apps.stop().await;
    for t in tasks {
        let _ = t.await;
    }
    // The last lines, "stopping" among them, reach the store.
    if let Some(s) = store {
        tracing::info!("stopping");
        s.close();
    }
}

/// The OpenSearch API of `svc` ([`server::serve_opensearch`]).
async fn serve_opensearch(svc: std::sync::Arc<chloedb_server::service::Service>, l: tokio::net::TcpListener, access: Access, version: String) {
    if let Err(e) = server::serve_opensearch(svc, l, access, &version).await {
        tracing::error!(error = %e, "OpenSearch API stopped");
    }
}

/// What `chloedb-server admin` asks ([`chloedb_server::admin_cli`]).
#[derive(Subcommand)]
enum AdminCmd {
    /// The key: its scope, role, and what it may do (`may`).
    Whoami,
    /// The fleet: its problems, each with what to do next.
    Health,
    /// The routes (with their scope, tier, dry run and MCP tool) and the
    /// error codes; words narrow the routes by path.
    Catalog { q: Option<String> },
    /// What runs in the background: plans, moves, on-call runs, change
    /// requests.
    Jobs,
    /// The latest entries of the audit log.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u64,
    },
    /// The change requests (or one, by id).
    Changes {
        id: Option<String>,
        #[arg(long)]
        state: Option<String>,
    },
    /// The runbook for an alert's rule or a fleet problem's id.
    Runbook { id: String },
    /// Any request: METHOD PATH [BODY] (JSON, `-` for stdin, `@file`).
    Call { method: String, path: String, body: Option<String> },
    /// The dry run of a change: METHOD PATH [BODY].
    Preview { method: String, path: String, body: Option<String> },
    /// File a change request for a person to approve: METHOD PATH [BODY]
    /// with --reason.
    Propose {
        method: String,
        path: String,
        body: Option<String>,
        /// What it does, in words (the dry run's, by default).
        #[arg(long)]
        what: Option<String>,
    },
}

fn fail(msg: &str) -> ! {
    tracing::error!("{msg}");
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    /// A new option goes under a topic of `--help`, or it would be listed
    /// apart from the rest, under "Options".
    #[test]
    fn every_option_is_under_a_help_topic() {
        let mut cmd = super::args_command();
        cmd.build();
        let loose: Vec<String> = cmd
            .get_arguments()
            .filter(|a| !matches!(a.get_id().as_str(), "help" | "version") && a.get_help_heading().is_none())
            .map(|a| a.get_id().to_string())
            .collect();
        assert!(loose.is_empty(), "options under no topic of HELP_TOPICS in main.rs: {loose:?}");
        let mut seen = std::collections::HashSet::new();
        for id in super::HELP_TOPICS.iter().flat_map(|(_, ids)| ids.iter()) {
            assert!(seen.insert(id), "{id} is under two topics");
        }
    }
}
