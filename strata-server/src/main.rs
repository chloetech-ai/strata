//! `strata-server`: Strata over HTTP JSON and gRPC, with a query UI.
//!
//! ```text
//! strata-server --demo 20000 --db products.strata      # a demo catalogue, UI at http://127.0.0.1:7700
//! strata-server --db my.strata                         # an existing database
//! strata-server --shards http://a:7701,http://b:7701   # a coordinator over shard servers
//! strata-server --shards 'http://a0:7701|http://a1:7701,http://b0:7701|http://b1:7701'   # two replicas each
//! strata-server --db new.strata --schema new.json --copy-from my.strata   # re-embed into a new schema
//! strata-server fetch-model potion-retrieval-32M       # download an embedding model
//! ```

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use strata_server::server::{self, Options};

#[derive(Parser)]
#[command(about = "Strata over HTTP JSON and gRPC, with a query UI and an agent", args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The database to serve (created if missing, with --schema or --demo).
    #[arg(long)]
    db: Option<PathBuf>,
    /// Schema of a new database, as JSON.
    #[arg(long)]
    schema: Option<PathBuf>,
    /// Create the demo product catalogue with this many products.
    #[arg(long)]
    demo: Option<usize>,
    /// With --demo: keep only the products of shard I of N ("I/N").
    #[arg(long)]
    demo_shard: Option<String>,
    /// Fill the new database (--db with --schema) with this database's rows,
    /// field by field by name; vector fields whose embedding changed are
    /// embedded anew.
    #[arg(long)]
    copy_from: Option<PathBuf>,
    /// Where embedded fields' models are (see `fetch-model`).
    #[arg(long, env = "STRATA_MODELS")]
    models: Option<PathBuf>,
    /// Create a new database as a directory of segment files.
    #[arg(long)]
    directory: bool,
    /// Coordinate these shard servers (comma-separated gRPC URLs; replicas
    /// of one shard joined by '|').
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
    /// With --shards: answer without shards that have no replica answering,
    /// flagged "partial" (a request's "partial" overrides it).
    #[arg(long)]
    partial: bool,
    /// HTTP address (API, metrics, UI).
    #[arg(long, default_value = "127.0.0.1:7700")]
    listen: String,
    /// gRPC address ("off" to disable).
    #[arg(long, default_value = "127.0.0.1:7701")]
    grpc: String,
    /// Require "Authorization: Bearer TOKEN" on the API (the UI asks for it).
    #[arg(long, env = "STRATA_TOKEN")]
    token: Option<String>,
    /// Default query deadline.
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,
    /// Queries running at once; more wait for a slot until their deadline.
    #[arg(long, default_value_t = 64)]
    max_concurrent: usize,
    /// The agent's default model.
    #[arg(long, default_value = strata_server::agent::DEFAULT_MODEL)]
    model: String,
    /// The Claude API's base URL.
    #[arg(long, env = "ANTHROPIC_BASE_URL", default_value = "https://api.anthropic.com")]
    api_base: String,
}

#[derive(Subcommand)]
enum Command {
    /// Download an embedding model from Hugging Face into the models directory.
    FetchModel {
        /// A known name (potion-retrieval-32M, potion-base-8M, bge-small-en-v1.5,
        /// snowflake-arctic-embed-m-v1.5) or a repository, owner/name.
        name: String,
        /// Where to put it (default: --models, $STRATA_MODELS, or
        /// ~/.local/share/strata/models).
        #[arg(long, env = "STRATA_MODELS")]
        models: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    let a = Args::parse();
    if let Some(Command::FetchModel { name, models }) = &a.command {
        let dir = models.clone().unwrap_or_else(strata::embed::models_dir);
        match server::fetch_model(name, &dir).await {
            Ok(path) => eprintln!("strata-server: {name} is in {}", path.display()),
            Err(e) => fail(&e),
        }
        return;
    }
    let demo_shard = a.demo_shard.as_deref().map(|s| {
        let (i, n) = s.split_once('/').unwrap_or_else(|| fail("--demo-shard is I/N"));
        (i.parse().unwrap_or_else(|_| fail("--demo-shard is I/N")), n.parse().unwrap_or_else(|_| fail("--demo-shard is I/N")))
    });
    let o = Options {
        db: a.db,
        schema: a.schema,
        copy_from: a.copy_from,
        demo: a.demo,
        demo_shard,
        directory: a.directory,
        shards: a.shards,
        coord: strata_server::coord::CoordOptions {
            hedge: std::time::Duration::from_millis(a.hedge_ms),
            shard_timeout: (a.shard_timeout_ms > 0).then(|| std::time::Duration::from_millis(a.shard_timeout_ms)),
            partial: a.partial,
            ..Default::default()
        },
        timeout_ms: a.timeout_ms,
        max_concurrent: a.max_concurrent,
        model: a.model,
        api_key: std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()),
        api_base: a.api_base,
        models: a.models.unwrap_or_else(strata::embed::models_dir),
    };
    let coordinator = !o.shards.is_empty();
    let svc = server::service(&o).await.unwrap_or_else(|e| fail(&e));
    let http = tokio::net::TcpListener::bind(&a.listen).await.unwrap_or_else(|e| fail(&format!("listen on {}: {e}", a.listen)));
    let h = svc.health().await;
    eprintln!("strata-server: table {} ({} rows, {} segments)", svc.schema.table, h["docs"], h["segments"]);
    let ui = match &a.token {
        Some(t) => format!("http://{}/?token={t}", a.listen),
        None => format!("http://{}/", a.listen),
    };
    eprintln!("strata-server: HTTP and UI on {ui}");
    if svc.agent.api_key.is_none() {
        eprintln!("strata-server: no ANTHROPIC_API_KEY; the agent asks for a key in the UI");
    }
    let mut tasks = Vec::new();
    // A coordinator answers queries but is not itself a shard.
    if a.grpc != "off" && !coordinator {
        let l = tokio::net::TcpListener::bind(&a.grpc).await.unwrap_or_else(|e| fail(&format!("listen on {}: {e}", a.grpc)));
        eprintln!("strata-server: gRPC on {}", a.grpc);
        let svc = svc.clone();
        tasks.push(tokio::spawn(async move {
            if let Err(e) = server::serve_grpc(svc, l).await {
                eprintln!("strata-server: gRPC stopped: {e}");
            }
        }));
    }
    if let Err(e) = server::serve_http(svc, http, a.token).await {
        fail(&format!("HTTP server: {e}"));
    }
    for t in tasks {
        let _ = t.await;
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("strata-server: {msg}");
    std::process::exit(1)
}
