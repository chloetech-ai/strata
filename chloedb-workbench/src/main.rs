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
// The rewrite (JOURNEYS.md, "Rules and notifications"): suggested rules and the interrupt model.
mod notify;
mod rulesuggest;
// The rewrite around goals (JOURNEYS.md): the one Needs-you count, and what
// sessions were allowed without asking.
mod needs;
// Optimize the harness: versions, findings, the permissions and memory editors.
mod harness;
// Why a session ended, in plain words.
mod endings;
mod killguard;

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
    /// With a PATH, that project is opened (and the URL goes to it).
    Serve(Serve),
    /// Starts a new project in DIR (made and `git init`ed when it is not a
    /// repository yet) and prints where to describe it.
    New(NewArgs),
    /// Prints the running server's URL for a project: its Continue page,
    /// or with --review, --debug or --continue that goal.
    Open(OpenArgs),
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
    /// A project to open: its folder (a git checkout, or any folder).
    path: Option<PathBuf>,
    /// The page the printed URL opens (for `new`).
    #[arg(skip)]
    route: Option<String>,
}

#[derive(Parser)]
struct NewArgs {
    /// The new project's folder (default: the current folder).
    dir: Option<PathBuf>,
    /// The running server's data directory (default ~/.local/share/chloedb-workbench).
    #[arg(long, env = "CHLOEDB_WORKBENCH_HOME")]
    data_dir: Option<PathBuf>,
}

#[derive(Parser)]
struct OpenArgs {
    /// The project's folder (default: the current folder).
    path: Option<PathBuf>,
    /// Continue: what happened since you left (the default).
    #[arg(long = "continue", conflicts_with_all = ["review", "debug"])]
    cont: bool,
    /// Review and ship what is ready.
    #[arg(long, conflicts_with = "debug")]
    review: bool,
    /// Debug: why the code is like this (the question Q).
    #[arg(long, value_name = "Q")]
    debug: Option<String>,
    /// The running server's data directory (default ~/.local/share/chloedb-workbench).
    #[arg(long, env = "CHLOEDB_WORKBENCH_HOME")]
    data_dir: Option<PathBuf>,
}

fn default_data_dir(d: Option<PathBuf>) -> Result<PathBuf, String> {
    match d {
        Some(d) => Ok(d),
        None => {
            let home = std::env::var_os("HOME").ok_or("HOME is not set; pass --data-dir")?;
            Ok(PathBuf::from(home).join(".local/share/chloedb-workbench"))
        }
    }
}

/// The running server, from `<data dir>/server.json`, when it answers.
fn running_server(data_dir: &std::path::Path) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(data_dir.join("server.json")).ok()?).ok()?;
    let (url, token) = (v["url"].as_str()?.to_string(), v["token"].as_str()?.to_string());
    let res = hook::agent(std::time::Duration::from_secs(2)).get(format!("{url}/api/health")).call().ok()?;
    (res.status().as_u16() == 200).then_some((url, token))
}

/// Opens a folder as a project on the running server: its short name.
fn open_on(url: &str, token: &str, path: &std::path::Path) -> Result<String, String> {
    let body = serde_json::json!({ "path": path.to_string_lossy() }).to_string();
    let mut res = hook::agent(std::time::Duration::from_secs(10))
        .post(format!("{url}/api/repos"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .send(body.as_bytes())
        .map_err(|e| format!("cannot reach the server at {url}: {e}"))?;
    let text = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| format!("the server said: {text}"))?;
    if let Some(e) = v["error"]["message"].as_str().or(v["message"].as_str()).filter(|_| v["repo"].is_null()) {
        return Err(e.to_string());
    }
    Ok(v["repo"]["project"].as_str().unwrap_or("").to_string())
}
fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn new_project(a: NewArgs) -> Result<(), String> {
    let dir = repos::init_project(&a.dir.unwrap_or_else(|| PathBuf::from(".")))?;
    let data_dir = default_data_dir(a.data_dir.clone())?;
    let route = format!("#/new/what?dir={}", enc(&dir.to_string_lossy()));
    if let Some((url, token)) = running_server(&data_dir) {
        open_on(&url, &token, &dir)?;
        println!("{url}/?token={token}{route}");
        return Ok(());
    }
    let mut s = Serve::try_parse_from(["serve"]).map_err(|e| e.to_string())?;
    s.data_dir = a.data_dir;
    s.path = Some(dir);
    s.route = Some(route);
    serve(s)
}

fn open_project(a: OpenArgs) -> Result<(), String> {
    let data_dir = default_data_dir(a.data_dir)?;
    let (url, token) = running_server(&data_dir).ok_or_else(|| format!("no Workbench is running for {} (start one with `chloedb-workbench serve`)", data_dir.display()))?;
    let path = a.path.unwrap_or_else(|| PathBuf::from("."));
    let path = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
    let project = open_on(&url, &token, &path)?;
    let route = if a.review {
        format!("#/p/{project}/ship")
    } else if let Some(q) = a.debug.as_deref().filter(|q| !q.trim().is_empty()) {
        format!("#/debug/code?q={}", enc(q.trim()))
    } else {
        let _ = a.cont;
        format!("#/p/{project}")
    };
    println!("{url}/?token={token}{route}");
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hook => {
            hook::run();
            std::process::exit(0);
        }
        Cmd::Mcp => mcp::run(),
        Cmd::Serve(s) => exit_on(serve(s)),
        Cmd::New(a) => exit_on(new_project(a)),
        Cmd::Open(a) => exit_on(open_project(a)),
    }
}

fn exit_on(r: Result<(), String>) {
    if let Err(e) = r {
        eprintln!("chloedb-workbench: {e}");
        std::process::exit(1);
    }
}

fn write_server_file(data_dir: &std::path::Path, url: &str, token: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let p = data_dir.join("server.json");
    let body = serde_json::json!({ "url": url, "token": token, "pid": std::process::id() }).to_string();
    let _ = std::fs::remove_file(&p);
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p) {
        let _ = f.write_all(body.as_bytes());
    }
}

fn serve(s: Serve) -> Result<(), String> {
    if !s.addr.ip().is_loopback() {
        return Err(format!("--addr {} is not a loopback address: Workbench listens on 127.0.0.1 or [::1] only", s.addr));
    }
    let data_dir = default_data_dir(s.data_dir)?;
    let open_path = match &s.path {
        Some(p) => Some(p.canonicalize().map_err(|e| format!("{}: {e}", p.display()))?),
        None => None,
    };
    let route = s.route.clone();
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
        // Where `open` and `new` find this server: readable by this user only.
        write_server_file(&data_dir, &url, &token);
        if let Some(p) = &open_path {
            let path = p.to_string_lossy().to_string();
            let sup2 = sup.clone();
            match tokio::task::spawn_blocking(move || repos::add_trusting(&sup2, &path, None)).await {
                Ok(Ok(r)) => {
                    let project = r["repo"]["project"].as_str().unwrap_or("");
                    let to = route.clone().unwrap_or_else(|| format!("#/p/{project}"));
                    println!("open: {url}/?token={token}{to}");
                }
                Ok(Err(e)) => eprintln!("chloedb-workbench: cannot open {}: {}", p.display(), e.message),
                Err(e) => eprintln!("chloedb-workbench: {e}"),
            }
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let lv = levels::Levels::new(&sup);
        server::serve(listener, server::App { sup, ui_dir, lv }).await.map_err(|e| e.to_string())
    })
}
