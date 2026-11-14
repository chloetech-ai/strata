//! `chloedb-crawl serve | crawl | preview`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde_json::json;

use chloedb_crawl::api::{self, Service};
use chloedb_crawl::extract::Rule;
use chloedb_crawl::grow::{Grower, How, Local, Policy};
use chloedb_crawl::job::{Phase, Spec};
use chloedb_crawl::strata::Strata;
use chloedb_crawl::urls::Scope;

#[derive(Parser)]
#[command(name = "chloedb-crawl", version, about = "A crawler on ChloeDB: a site's pages, their fields, a table inferred from them, and a cluster that grows as it fills")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the crawler's API, for the command center (chloedb-server --app crawler=URL) and for programs.
    Serve(ServeArgs),
    /// Crawl from the URLs given, print the progress, and exit when done.
    Crawl(CrawlArgs),
    /// Read a few pages from a URL and print the fields found and the table they would make; nothing is written.
    Preview(PreviewArgs),
}

#[derive(Args)]
struct Target {
    /// The chloedb-server (one server, or a cluster's coordinator) to write to.
    /// `--strata`, its name before the rename, works too.
    #[arg(long, alias = "strata", env = "CHLOEDB_URL")]
    server: Option<String>,
    /// Its API key (write scope; admin to grow a cluster).
    #[arg(long, env = "CHLOEDB_KEY", hide_env_values = true)]
    key: Option<String>,
    /// Start a cluster on this machine, with its databases and logs in this directory (again on the same directory: the same cluster).
    #[arg(long)]
    local: Option<PathBuf>,
    /// The chloedb-server binary for --local (default: next to this one, or on the PATH).
    #[arg(long, env = "CHLOEDB_SERVER_BIN")]
    server_bin: Option<PathBuf>,
    /// Where the --local cluster's coordinator listens (and its command center).
    #[arg(long, default_value = "127.0.0.1:7700")]
    local_listen: String,
    /// The first port of the --local shards: shard i listens on base+10i (HTTP) and base+10i+1 (gRPC).
    #[arg(long, default_value_t = 7710)]
    port_base: u16,
    /// How the cluster grows: off, local (servers it starts, with --local), kube (the StrataCluster's operator), or servers=URL,URL (empty servers started for it, by their gRPC URLs). Default: local with --local, else off.
    #[arg(long, default_value = "auto")]
    grow: String,
    /// Rows of the crawl's tables on a shard past which the cluster gets one more shard (0: never).
    #[arg(long, default_value_t = 100_000)]
    split_rows: u64,
    /// The most shards growing makes.
    #[arg(long, default_value_t = 8)]
    max_shards: usize,
}

/// What the crawler may reach beyond the public internet.
#[derive(Args, Clone)]
struct ReachArgs {
    /// Fetch from private, loopback and link-local addresses too (an intranet crawl). Without it, and without --allow-host, the crawler reaches the public internet only.
    #[arg(long, env = "CHLOEDB_CRAWL_ALLOW_PRIVATE")]
    allow_private: bool,
    /// A host (HOST or HOST:PORT) reachable whatever its addresses, such as an intranet site (repeatable).
    #[arg(long)]
    allow_host: Vec<String>,
}

impl ReachArgs {
    fn set(&self) {
        let hosts = chloedb_crawl::reach::Reach::parse_hosts(&self.allow_host).unwrap_or_else(|e| fail(e));
        chloedb_crawl::reach::set(chloedb_crawl::reach::Reach { private: self.allow_private, hosts });
    }
}

#[derive(Args, Clone)]
struct SpecArgs {
    /// Where to start.
    #[arg(required = true)]
    urls: Vec<String>,
    /// The table (default: the first URL's host, as books_toscrape_com).
    #[arg(long)]
    table: Option<String>,
    /// host, domain (its subdomains too), prefix (below the URL's path) or any.
    #[arg(long, default_value = "host")]
    scope: String,
    /// Follow only links matching this regular expression (repeatable).
    #[arg(long)]
    include: Vec<String>,
    /// Never follow links matching this (repeatable).
    #[arg(long)]
    exclude: Vec<String>,
    /// Make rows only of pages matching this (repeatable; default all).
    #[arg(long)]
    keep: Vec<String>,
    #[arg(long, default_value_t = 1000)]
    max_pages: u64,
    #[arg(long, default_value_t = 5)]
    max_depth: u32,
    /// Requests at once, over all hosts.
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    /// Requests at once to one host (the crawler's promise to the sites it
    /// reads, as in the command center: one at a time, a second apart; a
    /// site you run can take more).
    #[arg(long, default_value_t = 1)]
    per_host: usize,
    /// Milliseconds between two requests to a host.
    #[arg(long, default_value_t = 1000)]
    delay_ms: u64,
    /// Ignore robots.txt (only for sites you own).
    #[arg(long)]
    no_robots: bool,
    #[arg(long)]
    no_sitemaps: bool,
    /// Pages read before the table is made.
    #[arg(long, default_value_t = 30)]
    sample: usize,
    /// The share of pages a field needs to be a column.
    #[arg(long, default_value_t = 0.3)]
    promote_at: f64,
    /// A field from a CSS selector: name=selector, name=selector@attr, or name[]=selector for every match (repeatable).
    #[arg(long)]
    rule: Vec<String>,
    /// Do not learn fields from the site's templates.
    #[arg(long)]
    no_learn: bool,
    /// Embed the title and body into a vector field emb with this model (as potion-base-8M).
    #[arg(long)]
    embed: Option<String>,
    /// The crawl's own user agent (the crawler's name and version are added after it).
    #[arg(long)]
    user_agent: Option<String>,
    /// Whom a site's owner reaches about this crawl: an email address or a URL, sent in the user agent.
    #[arg(long, env = "CHLOEDB_CRAWL_CONTACT")]
    contact: Option<String>,
}

impl SpecArgs {
    fn spec(&self) -> Result<Spec, String> {
        let rules = self.rule.iter().map(|r| Rule::parse(r)).collect::<Result<Vec<_>, _>>()?;
        let mut spec = api::default_spec();
        spec.seeds = self.urls.clone();
        spec.table = self.table.clone();
        spec.scope = Scope::parse(&self.scope).ok_or_else(|| format!("--scope {:?}: host, domain, prefix or any", self.scope))?;
        spec.include = self.include.clone();
        spec.exclude = self.exclude.clone();
        spec.keep = self.keep.clone();
        spec.max_pages = self.max_pages;
        spec.max_depth = self.max_depth;
        spec.concurrency = self.concurrency;
        spec.per_host = self.per_host;
        spec.delay_ms = self.delay_ms;
        spec.robots = !self.no_robots;
        spec.sitemaps = !self.no_sitemaps;
        spec.sample = self.sample;
        spec.promote_at = self.promote_at;
        spec.rules = rules;
        spec.learn = !self.no_learn;
        spec.embed = self.embed.clone();
        spec.user_agent = self.user_agent.clone();
        spec.contact = self.contact.clone();
        spec.check()?;
        Ok(spec)
    }
}

#[derive(Args)]
struct ServeArgs {
    /// Where the API listens.
    #[arg(long, default_value = "127.0.0.1:7800")]
    listen: String,
    /// A token every request must carry (Authorization: Bearer); chloedb-server sends its CHLOEDB_APP_TOKEN. With --local and no token, one is made for the coordinator.
    #[arg(long, env = "CHLOEDB_CRAWL_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// The contact of crawls that give none: whom a site's owner reaches (an email address or a URL), sent in the user agent.
    #[arg(long, env = "CHLOEDB_CRAWL_CONTACT")]
    contact: Option<String>,
    /// What the name of every table a crawl fills starts with (crawl_: a crawl of books.toscrape.com fills crawl_books_toscrape_com).
    #[arg(long, env = "CHLOEDB_CRAWL_TABLE_PREFIX")]
    table_prefix: Option<String>,
    #[command(flatten)]
    reach: ReachArgs,
    #[command(flatten)]
    target: Target,
}

#[derive(Args)]
struct CrawlArgs {
    #[command(flatten)]
    spec: SpecArgs,
    #[command(flatten)]
    reach: ReachArgs,
    #[command(flatten)]
    target: Target,
    /// With --local: keep the cluster serving after the crawl, until Ctrl-C.
    #[arg(long)]
    stay: bool,
}

#[derive(Args)]
struct PreviewArgs {
    #[command(flatten)]
    spec: SpecArgs,
    #[command(flatten)]
    reach: ReachArgs,
    /// Pages to read: the URL and some it links to.
    #[arg(long, default_value_t = 6)]
    pages: usize,
}

fn fail(m: impl std::fmt::Display) -> ! {
    eprintln!("chloedb-crawl: {m}");
    std::process::exit(1)
}

/// The chloedb-server binary: --server-bin, else next to this binary, else on the PATH.
fn server_bin(given: &Option<PathBuf>) -> PathBuf {
    if let Some(b) = given {
        return b.clone();
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        let p = dir.join("chloedb-server");
        if p.exists() {
            return p;
        }
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let p = dir.join("chloedb-server");
        if p.exists() {
            return p;
        }
    }
    fail("--local needs chloedb-server: give --server-bin, or put it on the PATH (cd chloedb-server && cargo build --release)")
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    } else {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        b = (t ^ (std::process::id() as u128) << 64).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// ChloeDB to write to, the local cluster (started) if any, and the grower.
async fn connect(t: &Target, app: Option<(String, Option<String>)>) -> (Strata, Option<Arc<Local>>, Arc<Grower>) {
    let (url, local) = match (&t.local, &t.server) {
        (Some(_), Some(_)) => fail("give --server or --local, not both"),
        (Some(dir), None) => {
            let l = Local::new(dir.clone(), server_bin(&t.server_bin), t.local_listen.clone(), t.port_base, app);
            let url = l.start().await.unwrap_or_else(|e| fail(e));
            eprintln!("chloedb-crawl: a cluster in {} answers at {url} (its command center too)", dir.display());
            (url, Some(l))
        }
        (None, url) => (url.clone().unwrap_or_else(|| "http://127.0.0.1:7700".into()), None),
    };
    let strata = Strata::new(&url, t.key.clone());
    if local.is_none() {
        if let Err(e) = strata.health().await {
            eprintln!("chloedb-crawl: warning: {url} does not answer yet ({e})");
        }
    }
    let how = match t.grow.as_str() {
        "auto" if local.is_some() => How::Local,
        "auto" | "off" => How::Off,
        "local" if local.is_some() => How::Local,
        "local" => fail("--grow local needs --local"),
        "kube" => How::Kube,
        s => match s.strip_prefix("servers=") {
            Some(list) => How::Servers { servers: list.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect() },
            None => fail(format!("--grow {s:?}: off, local, kube or servers=URL,URL")),
        },
    };
    let grower = Grower::new(Policy { how, split_rows: t.split_rows, max_shards: t.max_shards.max(1) }, strata.clone(), local.clone());
    tokio::spawn(grower.clone().run());
    (strata, local, grower)
}

/// Resolves on SIGINT or SIGTERM. The handlers are installed when it is
/// called, not when it is first awaited: called before a local cluster
/// starts, a signal that comes while the cluster is starting (its
/// coordinator already answers /v1/health then) is kept, and the cluster is
/// stopped, rather than the process dying of the signal's default action
/// and leaving its chloedb-servers running.
fn shutdown_signal() -> impl std::future::Future<Output = ()> + Send + 'static {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("a signal handler");
        let mut int = signal(SignalKind::interrupt()).expect("a signal handler");
        async move {
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        }
    }
    #[cfg(not(unix))]
    async {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn main() {
    // Settings may be named CHLOEDB_* (the name since the rename) or STRATA_*:
    // clap and the code read the old names, so the new ones are copied over
    // before the runtime starts any thread.
    chloedb_crawl::adopt_old_env_names();
    run();
}

#[tokio::main]
async fn run() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "chloedb_crawl=info".into())).with_writer(std::io::stderr).with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr())).init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Preview(a) => {
            a.reach.set();
            let spec = a.spec.spec().unwrap_or_else(|e| fail(e));
            match chloedb_crawl::job::preview(&spec, a.pages.max(1)).await {
                Ok(j) => println!("{}", serde_json::to_string_pretty(&j).expect("json")),
                Err(e) => fail(e),
            }
        }
        Cmd::Serve(a) => {
            a.reach.set();
            let token = a.token.clone().or_else(|| a.target.local.as_ref().map(|_| random_token()));
            // Started by chloedb-server: the socket it holds, not --listen.
            let listener = match chloedb_crawl::inherited_listener().unwrap_or_else(|e| fail(e)) {
                Some(l) => l.set_nonblocking(true).and_then(|()| tokio::net::TcpListener::from_std(l)).unwrap_or_else(|e| fail(format!("the inherited socket: {e}"))),
                None => tokio::net::TcpListener::bind(&a.listen).await.unwrap_or_else(|e| fail(format!("{}: {e}", a.listen))),
            };
            let me = format!("http://{}", listener.local_addr().map(|x| x.to_string()).unwrap_or(a.listen.clone()));
            // Before the cluster starts: see shutdown_signal.
            let stop = shutdown_signal();
            let (strata, local, grower) = connect(&a.target, Some((me.clone(), token.clone()))).await;
            if let Some(c) = a.contact.as_deref().filter(|c| !chloedb_crawl::fetch::header_text(c)) {
                fail(format!("--contact {c:?}: printable ASCII, at most 300 characters"));
            }
            if let Some(p) = a.table_prefix.as_deref().filter(|p| !p.is_empty() && !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || !p.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')) {
                fail(format!("--table-prefix {p:?}: letters, digits and _, starting with a letter"));
            }
            let svc = Service::with_prefix(strata.clone(), grower, token.clone(), local.is_some(), a.contact.clone(), a.table_prefix.clone());
            eprintln!("chloedb-crawl: the API answers at {me}{}", if token.is_some() && a.token.is_none() { " (token made for the coordinator)" } else { "" });
            // A chloedb-server that started it (CHLOEDB_APP_OF) shows it already.
            if local.is_none() && std::env::var_os("CHLOEDB_APP_OF").is_none() {
                eprintln!("chloedb-crawl: to see it in the command center, start the coordinator with --app crawler={me}{}", if token.is_some() { " and CHLOEDB_APP_TOKEN set to the crawler's token" } else { "" });
            }
            let svc2 = svc.clone();
            let server = axum::serve(listener, api::router(svc)).with_graceful_shutdown(async move {
                stop.await;
                eprintln!("chloedb-crawl: stopping: the crawls write what they read");
                svc2.stop_all(30).await;
            });
            if let Err(e) = server.await {
                eprintln!("chloedb-crawl: {e}");
            }
            if let Some(l) = local {
                l.stop().await;
            }
        }
        Cmd::Crawl(a) => {
            a.reach.set();
            let spec = a.spec.spec().unwrap_or_else(|e| fail(e));
            let mut stop = Box::pin(shutdown_signal());
            let (strata, local, grower) = connect(&a.target, None).await;
            let svc = Service::new(strata, grower.clone(), None, local.is_some());
            let job = svc.start(spec).unwrap_or_else(|e| fail(e));
            let mut stopping = false;
            loop {
                tokio::select! {
                    _ = &mut stop, if !stopping => { stopping = true; eprintln!("chloedb-crawl: stopping"); job.stop(); }
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                let s = job.status();
                let g = grower.status();
                let shards: Vec<String> = g["shards"].as_array().into_iter().flatten().map(|s| s["rows"].to_string()).collect();
                eprintln!(
                    "{:>9}  fetched {:>7}  written {:>7}  queued {:>7}  {:>6.1}/s  {}{}",
                    s["phase"].as_str().unwrap_or(""),
                    s["counts"]["fetched"], s["counts"]["written"], s["frontier"]["queued"],
                    s["rate"]["fetched"].as_f64().unwrap_or(0.0),
                    if shards.is_empty() { String::new() } else { format!("shards {}", shards.join("/")) },
                    if g["moving"] == true { " (moving slots)" } else { "" },
                );
                if !job.active() {
                    break;
                }
            }
            let s = job.status();
            for e in s["events"].as_array().into_iter().flatten() {
                eprintln!("  {}: {}", e["kind"].as_str().unwrap_or(""), e["text"].as_str().unwrap_or(""));
            }
            for e in grower.status()["events"].as_array().into_iter().flatten() {
                eprintln!("  {}: {}", e["kind"].as_str().unwrap_or(""), e["text"].as_str().unwrap_or(""));
            }
            if let Some(sql) = s["sql"].as_str() {
                eprintln!("\n{sql}\n");
            }
            println!("{}", serde_json::to_string(&json!({"table": s["table"], "phase": s["phase"], "counts": s["counts"], "error": s["error"]})).expect("json"));
            if let Some(l) = local {
                if a.stay {
                    eprintln!("chloedb-crawl: the cluster serves at {} until Ctrl-C", l.url());
                    shutdown_signal().await;
                }
                l.stop().await;
            }
            if job.phase() == Phase::Failed {
                std::process::exit(1);
            }
        }
    }
}
