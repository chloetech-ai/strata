//! Opening the database, and serving HTTP and gRPC.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use strata::db::Db;
use strata::ingest::Format;
use strata::schema::Schema;
use strata::segment::build::BuildOptions;
use strata::storage::{DbOptions, Layout, MergePolicy};
use tokio::net::TcpListener;

use crate::auth::{self, Access};
use crate::coord::{CoordOptions, Coordinator};
use crate::service::{shard_of, AgentConfig, Backend, Service};
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
    /// Coordinate these shard servers (gRPC URLs; replicas of a shard
    /// joined by `|`) instead of serving a database.
    pub shards: Vec<String>,
    pub coord: CoordOptions,
    pub timeout_ms: u64,
    /// The longest deadline a request may ask for (0: the larger of
    /// `timeout_ms` and 60 s).
    pub max_timeout_ms: u64,
    pub max_concurrent: usize,
    pub model: String,
    pub api_key: Option<String>,
    pub api_base: String,
    /// Where embedded fields' models are ([`strata::embed::open`]).
    pub models: PathBuf,
    /// The synonym list for `synonyms => 1` searches (Solr format), read at
    /// start if it exists and written by `PUT /v1/synonyms`.
    pub synonyms: Option<PathBuf>,
    /// Where `POST /v1/admin/checkpoint` writes checkpoints.
    pub backup_dir: Option<PathBuf>,
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
    let (backend, schema) = if !o.shards.is_empty() {
        // A shard with no replica answering yet (the cluster is starting, or
        // one is down) is waited for; a wrong URL or schema is not.
        let c = loop {
            match Coordinator::connect_with(o.shards.clone(), o.coord.clone()).await {
                Ok(c) => break c,
                Err(e) if e.status != 400 => {
                    eprintln!("strata-server: waiting for the shards: {}", e.message);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => return Err(e.message),
            }
        };
        let schema = c.schema.clone();
        (Backend::Cluster(Arc::new(c)), schema)
    } else {
        let path = o.db.clone().ok_or("pass --db PATH (or --shards to coordinate shard servers)")?;
        if o.copy_from.is_some() && (path.exists() || o.schema.is_none()) {
            return Err("--copy-from fills a new database: pass a --db that does not exist and its --schema".into());
        }
        let db = if path.exists() {
            let db = Db::open(&path, DbOptions { wal_drop_damaged: o.wal_drop_damaged, verify_checksums: o.verify_checksums, ..db_options(o.directory) }).map_err(|e| format!("open {}: {e}", path.display()))?;
            if let Some(what) = db.storage().damaged() {
                eprintln!("strata-server: {} opened past damage: {what}", path.display());
            }
            db
        } else if let Some(n) = o.demo {
            fill = Some(Fill::Demo(n));
            Db::create(&path, demo::schema(n), db_options(o.directory)).map_err(|e| e.to_string())?
        } else if let Some(s) = &o.schema {
            let text = std::fs::read_to_string(s).map_err(|e| format!("read {}: {e}", s.display()))?;
            let schema = Schema::from_json(&text).map_err(|e| format!("{}: {e}", s.display()))?;
            fill = o.copy_from.clone().map(Fill::Copy);
            Db::create(&path, schema, db_options(o.directory)).map_err(|e| e.to_string())?
        } else {
            return Err(format!("{} does not exist: pass --schema FILE or --demo N to create it", path.display()));
        };
        let db = Arc::new(db);
        // Writes go on while a full memtable becomes segments.
        db.spawn_flusher();
        let schema = db.schema().clone();
        (Backend::Local(db), schema)
    };
    let mut svc = Service::new(backend, schema, agent, o.max_concurrent.max(1), timeout);
    if o.max_timeout_ms > 0 {
        svc.max_timeout = Duration::from_millis(o.max_timeout_ms).max(timeout);
    }
    svc.open_models(&o.models);
    for m in &svc.models {
        match &m.model {
            Ok(e) => eprintln!("strata-server: {} is {}'s embedding of {}", m.field, e.name(), m.spec.from.join(" + ")),
            Err(why) => eprintln!("strata-server: {} is {}'s embedding of {}, which this server cannot run ({why}): writes must supply its vectors", m.field, m.spec.model, m.spec.from.join(" + ")),
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
                let old = Db::open(&from, DbOptions::default()).map_err(|e| format!("open {}: {e}", from.display()))?;
                db.load_from(&old).map_err(|e| format!("copy from {}: {e}", from.display()))?
            }
        };
        eprintln!("strata-server: loaded {n} rows in {:.1} s", t0.elapsed().as_secs_f64());
    }
    if let Some(path) = &o.synonyms {
        svc.open_synonyms(path).map_err(|e| e.message)?;
    }
    if let Some(dir) = &o.backup_dir {
        svc.set_backup_dir(dir).map_err(|e| e.message)?;
    }
    let svc = Arc::new(svc);
    spawn_maintenance(&svc);
    Ok(svc)
}

/// Downloads model `name` ([`strata::embed::KNOWN_MODELS`], or a Hugging
/// Face repository `owner/name`) into `dir`; returns its directory.
pub async fn fetch_model(name: &str, dir: &std::path::Path) -> Result<PathBuf, String> {
    let (local, repo) = match strata::embed::KNOWN_MODELS.iter().find(|m| m.0.eq_ignore_ascii_case(name)) {
        Some(m) => (m.0.to_string(), m.1.to_string()),
        None if name.contains('/') => (name.rsplit('/').next().unwrap_or(name).to_string(), name.to_string()),
        None => {
            let known: Vec<&str> = strata::embed::KNOWN_MODELS.iter().map(|m| m.0).collect();
            return Err(format!("unknown model {name}: one of {}, or a Hugging Face repository owner/name", known.join(", ")));
        }
    };
    let out = dir.join(&local);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let client = reqwest::Client::new();
    for file in strata::embed::MODEL_FILES {
        let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");
        let r = client.get(&url).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("{url}: {e}"))?;
        let bytes = r.bytes().await.map_err(|e| format!("{url}: {e}"))?;
        // Written aside and renamed, so that an open model is never
        // overwritten in place.
        let tmp = out.join(format!("{file}.part"));
        std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, out.join(file))).map_err(|e| format!("{}: {e}", out.join(file).display()))?;
        eprintln!("strata-server: {file} ({:.1} MB)", bytes.len() as f64 / 1e6);
    }
    strata::embed::open(&local, dir).map_err(|e| e.to_string())?;
    Ok(out)
}

/// `strata-server import`: loads `files` into the database at `path`,
/// created with `schema` if it does not exist, and says what each wrote.
pub fn import_files(path: &Path, schema: Option<&Path>, models: &Path, format: Option<Format>, directory: bool, files: &[PathBuf]) -> Result<(), String> {
    let opts = DbOptions { layout: if directory { Layout::Directory } else { Layout::SingleFile }, build: build_options(), ..DbOptions::default() };
    let db = if path.exists() {
        if schema.is_some() {
            return Err(format!("{} exists: --schema is only for a new database", path.display()));
        }
        Db::open(path, opts).map_err(|e| format!("open {}: {e}", path.display()))?
    } else {
        let s = schema.ok_or_else(|| format!("{} does not exist: pass --schema FILE to create it", path.display()))?;
        let text = std::fs::read_to_string(s).map_err(|e| format!("read {}: {e}", s.display()))?;
        let schema = Schema::from_json(&text).map_err(|e| format!("{}: {e}", s.display()))?;
        Db::create(path, schema, opts).map_err(|e| format!("create {}: {e}", path.display()))?
    };
    for (field, why) in db.open_embedders(models) {
        eprintln!("strata-server: cannot embed {field} ({why}): its vectors must be in the files");
    }
    for file in files {
        let t0 = std::time::Instant::now();
        let r = strata::ingest::import_file(&db, file, format).map_err(|e| format!("{}: {e}", file.display()))?;
        let how = if r.bulk { "loaded" } else { "upserted" };
        eprintln!("strata-server: {}: {} rows {how} in {:.1} s", file.display(), r.rows, t0.elapsed().as_secs_f64());
        if !r.unknown.is_empty() {
            eprintln!("strata-server: {}: skipped fields the schema lacks: {}", file.display(), r.unknown.join(", "));
        }
    }
    db.storage().merge(&MergePolicy::default()).map_err(|e| format!("merge: {e}"))?;
    eprintln!("strata-server: {} has {} rows", path.display(), db.snapshot().n_live_docs());
    Ok(())
}

/// Merges a local database's segments in the background, as writes add
/// them, and releases expired snapshot leases. A database that cannot
/// write ([`Service::failing`]) is not merged: the merged segment could
/// not be committed.
pub fn spawn_maintenance(svc: &Arc<Service>) {
    let Some(db) = svc.local_db().cloned() else { return };
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut reported = false;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let Some(svc) = svc.upgrade() else { return };
            svc.sweep_leases();
            if let Some(why) = svc.failing() {
                if !reported {
                    eprintln!("strata-server: {why}; merges stop until then");
                    reported = true;
                }
                continue;
            }
            let db = db.clone();
            let r = tokio::task::spawn_blocking(move || db.storage().merge(&MergePolicy::default())).await;
            if let Ok(Err(e)) = r {
                eprintln!("strata-server: merge failed: {e}");
            }
        }
    });
}

/// Serves the HTTP API and UI, over TLS with `access.tls`.
pub async fn serve_http(svc: Arc<Service>, listener: TcpListener, access: Access) -> std::io::Result<()> {
    let router = http::router(svc, &access);
    let Some(tls) = &access.tls else {
        return axum::serve(listener, router).with_graceful_shutdown(shutdown()).await;
    };
    let config = auth::server_config(tls).map_err(std::io::Error::other)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let service = hyper_util::service::TowerToHyperService::new(router);
    let stop = shutdown();
    tokio::pin!(stop);
    loop {
        let (tcp, _) = tokio::select! {
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
        let (acceptor, service) = (acceptor.clone(), service.clone());
        tokio::spawn(async move {
            // A client that never finishes its handshake holds a task, not
            // the accept loop.
            let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await else { return };
            let io = hyper_util::rt::TokioIo::new(stream);
            let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new()).serve_connection_with_upgrades(io, service).await;
        });
    }
}

/// Serves gRPC: every call needs `access.token`, over TLS with
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
    let token = access.token.clone();
    let check = move |req: tonic::Request<()>| match &token {
        Some(t) if !t.admits(req.metadata().get("authorization").and_then(|v| v.to_str().ok())) => Err(tonic::Status::unauthenticated("missing or wrong token")),
        _ => Ok(req),
    };
    let service = pb::strata_server::StrataServer::new(grpc::Grpc(svc)).max_decoding_message_size(256 << 20).max_encoding_message_size(256 << 20);
    let mut builder = tonic::transport::Server::builder();
    if let Some(tls) = &access.tls {
        builder = builder.tls_config(tonic::transport::ServerTlsConfig::new().identity(tonic::transport::Identity::from_pem(&tls.cert, &tls.key)))?;
    }
    builder
        .add_service(tonic::service::interceptor::InterceptedService::new(service, check))
        .serve_with_incoming_shutdown(incoming, shutdown())
        .await
}

/// Ctrl-C, or SIGTERM: how Docker and Kubernetes stop a container. As a
/// container's first process the server would otherwise ignore SIGTERM, and
/// be killed only when the grace period ran out (30 s in Kubernetes).
async fn shutdown() {
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
