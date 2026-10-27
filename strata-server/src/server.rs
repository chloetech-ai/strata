//! Opening the database, and serving HTTP and gRPC.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use strata::db::Db;
use strata::schema::Schema;
use strata::segment::build::BuildOptions;
use strata::storage::{DbOptions, Layout, MergePolicy};
use tokio::net::TcpListener;

use crate::coord::Coordinator;
use crate::service::{shard_of, AgentConfig, Backend, Embedder, Service};
use crate::{demo, grpc, http, pb};

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The database to serve; created if missing (with `schema` or `demo`).
    pub db: Option<PathBuf>,
    /// A JSON schema (Strata's `Schema` serialization) for a new database.
    pub schema: Option<PathBuf>,
    /// Create the demo catalogue with this many products.
    pub demo: Option<usize>,
    /// Keep only the demo products of shard `i` of `n`.
    pub demo_shard: Option<(usize, usize)>,
    /// A new database as a directory of segment files.
    pub directory: bool,
    /// Coordinate these shard servers (gRPC URLs) instead of serving a database.
    pub shards: Vec<String>,
    pub timeout_ms: u64,
    pub max_concurrent: usize,
    pub model: String,
    pub api_key: Option<String>,
    pub api_base: String,
    /// Serve `{"embed": ...}` parameters with the demo's hashed vectors.
    pub hash_embedder: bool,
}

/// How the server opens a database. Each write publishes its snapshot,
/// rebuilding the newest in-memory chunk (up to 1,024 rows): those chunks
/// keep their vectors flat, since a graph over so few rows costs more to
/// build at every write than an exact scan of them costs a query.
pub fn db_options(directory: bool) -> DbOptions {
    DbOptions {
        publish_snapshots: true,
        mem_build: Some(BuildOptions::default().set("vector.index", "flat")),
        layout: if directory { Layout::Directory } else { Layout::SingleFile },
        ..DbOptions::default()
    }
}

/// Opens or creates the database, or connects to the shards.
pub async fn service(o: &Options) -> Result<Arc<Service>, String> {
    let agent = AgentConfig { api_key: o.api_key.clone(), model: o.model.clone(), base_url: o.api_base.clone() };
    let timeout = Duration::from_millis(o.timeout_ms.max(1));
    let (backend, schema) = if !o.shards.is_empty() {
        let c = Coordinator::connect(o.shards.clone()).await.map_err(|e| e.message)?;
        let schema = c.schema.clone();
        (Backend::Cluster(Arc::new(c)), schema)
    } else {
        let path = o.db.clone().ok_or("pass --db PATH (or --shards to coordinate shard servers)")?;
        let db = if path.exists() {
            Db::open(&path, db_options(o.directory)).map_err(|e| format!("open {}: {e}", path.display()))?
        } else if let Some(n) = o.demo {
            let db = Db::create(&path, demo::schema(n), db_options(o.directory)).map_err(|e| e.to_string())?;
            let keep = |i: usize| match o.demo_shard {
                Some((s, of)) => shard_of(&strata::Value::Str(format!("p{i:07}")), of) == s,
                None => true,
            };
            db.load((0..n).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).map_err(|e| e.to_string())?;
            db
        } else if let Some(s) = &o.schema {
            let text = std::fs::read_to_string(s).map_err(|e| format!("read {}: {e}", s.display()))?;
            let schema: Schema = serde_json::from_str(&text).map_err(|e| format!("schema {}: {e}", s.display()))?;
            Db::create(&path, schema, db_options(o.directory)).map_err(|e| e.to_string())?
        } else {
            return Err(format!("{} does not exist: pass --schema FILE or --demo N to create it", path.display()));
        };
        let db = Arc::new(db);
        spawn_maintenance(db.clone());
        let schema = db.schema().clone();
        (Backend::Local(db), schema)
    };
    let mut svc = Service::new(backend, schema, agent, o.max_concurrent.max(1), timeout);
    if o.hash_embedder || o.demo.is_some() {
        let dim = svc.vector_field().and_then(|f| match &svc.schema.field(&f)?.ty {
            strata::FieldType::Vector { dim, .. } => Some(*dim),
            _ => None,
        });
        if let Some(dim) = dim {
            svc.embedder = Some(Embedder { name: "hashed bag of words (the demo's; not a learned model)".into(), dim, embed: demo::hash_embed });
        }
    }
    Ok(Arc::new(svc))
}

/// Merges segments in the background, as writes add them.
pub fn spawn_maintenance(db: Arc<Db>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let db = db.clone();
            let r = tokio::task::spawn_blocking(move || db.storage().merge(&MergePolicy::default())).await;
            if let Ok(Err(e)) = r {
                eprintln!("strata-server: merge failed: {e}");
            }
        }
    });
}

pub async fn serve_http(svc: Arc<Service>, listener: TcpListener, token: Option<String>) -> std::io::Result<()> {
    axum::serve(listener, http::router(svc, token)).with_graceful_shutdown(shutdown()).await
}

pub async fn serve_grpc(svc: Arc<Service>, listener: TcpListener) -> Result<(), tonic::transport::Error> {
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
    tonic::transport::Server::builder()
        .add_service(pb::strata_server::StrataServer::new(grpc::Grpc(svc)).max_decoding_message_size(256 << 20).max_encoding_message_size(256 << 20))
        .serve_with_incoming_shutdown(incoming, shutdown())
        .await
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
