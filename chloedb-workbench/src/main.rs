//! ChloeDB Workbench: many Claude Code sessions side by side, each in a
//! real terminal and most often its own git worktree, with their state and
//! events as rows of a ChloeDB database. See SPEC.md.

mod blast;
mod capsule;
mod ctxwire;
mod db;
mod flow;
mod git;
mod hook;
mod map;
mod mcp;
mod preview;
mod recording;
mod server;
mod trail;
mod trace;
mod supervisor;
// The levels (SPEC.md, "Levels (third build)").
mod digest;
mod goals;
mod levels;
mod narrate;
mod questions;
mod story;
// Health (SPEC.md, "Fourth build", rows 4 and 5).
mod alerts;
mod testruns;
// Fourth build: time and guard (SPEC.md, rows 8, 9 and 6).
mod guarded;
mod routing;
mod timetravel;
// Search (SPEC.md, "Fourth build", rows 1 and 10).
mod library;
mod search;
// The Preview lab (SPEC.md, "Fourth build", use cases 2 and 3).
mod runtime;
mod swarm;
// Fifth build: Hotspots (SPEC.md).
mod hotspots;
// Fifth build: autocomplete and intent checks.
mod complete;
// Sixth build: repositories read without an agent.
mod repos;
// Sixth build: what a terminal shows, read for the prompt it waits on.
mod screen;
// The goals rewrite (JOURNEYS.md): Review and ship; Debug's turns and records.
mod review;
mod turns;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "chloedb-workbench", version, about = "Many Claude Code sessions side by side, their state in ChloeDB")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// The supervisor, the API and the UI; prints a URL with the token.
    Serve(Serve),
    /// What each session's hooks run: posts the hook's JSON to the server.
    Hook,
    /// An MCP server over stdio with spawn_agent and its siblings.
    Mcp,
}

#[derive(Parser)]
struct Serve {
    /// Where to listen: a loopback address only.
    #[arg(long, env = "CHLOEDB_WORKBENCH_ADDR", default_value = "127.0.0.1:7740")]
    addr: SocketAddr,
    /// The state database, the sessions' settings and the worktrees
    /// (default ~/.local/share/chloedb-workbench).
    #[arg(long, env = "CHLOEDB_WORKBENCH_HOME")]
    data_dir: Option<PathBuf>,
    /// The Claude Code program.
    #[arg(long, env = "CHLOEDB_WORKBENCH_CLAUDE", default_value = "claude")]
    claude_bin: String,
    /// Agents running at once.
    #[arg(long, default_value_t = 8)]
    max_agents: usize,
    /// How deep agents may start agents: with 2, an agent started by an
    /// agent started by you may start no more.
    #[arg(long, default_value_t = 2)]
    max_depth: i64,
    /// The ChloeCtx program the decision trail asks (`chloectx blame`,
    /// `sql`, `ask`); without it the trail answers from git and Workbench.
    #[arg(long, env = "CHLOEDB_WORKBENCH_CHLOECTX", default_value = "chloectx")]
    chloectx_bin: String,
    /// Start sessions without ChloeCtx (for tests): no hooks or memory
    /// tools from it, and no setup screen.
    #[arg(long, hide = true)]
    no_chloectx: bool,
    /// The most a session's terminal recording grows to, in bytes.
    #[arg(long, env = "CHLOEDB_WORKBENCH_RECORD_MAX", default_value_t = recording::MAX_BYTES, hide = true)]
    record_max_bytes: u64,
    /// The token every /api route wants (random when left out).
    #[arg(long, env = "CHLOEDB_WORKBENCH_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hook => {
            hook::run();
            std::process::exit(0);
        }
        Cmd::Mcp => mcp::run(),
        Cmd::Serve(s) => {
            if let Err(e) = serve(s) {
                eprintln!("chloedb-workbench: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn serve(s: Serve) -> Result<(), String> {
    if !s.addr.ip().is_loopback() {
        return Err(format!("--addr {} is not a loopback address: Workbench listens on 127.0.0.1 or [::1] only", s.addr));
    }
    let data_dir = match s.data_dir {
        Some(d) => d,
        None => {
            let home = std::env::var_os("HOME").ok_or("HOME is not set; pass --data-dir")?;
            PathBuf::from(home).join(".local/share/chloedb-workbench")
        }
    };
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);
    let token = s.token.filter(|t| !t.is_empty()).unwrap_or_else(|| supervisor::hex(32));
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(s.addr).await.map_err(|e| format!("cannot listen on {}: {e}", s.addr))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let url = format!("http://{addr}");
        let cfg = supervisor::Config { data_dir: data_dir.clone(), claude_bin: s.claude_bin, max_agents: s.max_agents, max_depth: s.max_depth, token: token.clone(), url: url.clone(), exe, chloectx_bin: ctxwire::resolve(&s.chloectx_bin), chloectx_required: !s.no_chloectx, record_max: s.record_max_bytes };
        let sup = supervisor::Supervisor::open(cfg)?;
        let ui_dir = std::env::var_os("CHLOEDB_WORKBENCH_UI_DIR").map(PathBuf::from);
        println!("Workbench: {url}/?token={token}");
        println!("state: {}", data_dir.join("workbench.chloedb").display());
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let lv = levels::Levels::new(&sup);
        server::serve(listener, server::App { sup, ui_dir, lv }).await.map_err(|e| e.to_string())
    })
}
