//! engram: automatic long-term context for Claude Code, stored in Strata.
//!
//!   engram install [--project DIR] [--shared]   wire hooks + MCP into a project
//!   engram hook <Event>                          (called by Claude Code hooks)
//!   engram mcp                                   (MCP stdio server, started by Claude Code)
//!   engram search "query" | sql "SELECT ..." | stats | briefing | stop
//!   engram remember <kind> "text"
//!   engram ingest-transcript <file.jsonl>        back-fill from a Claude Code transcript
//!   engram fetch-model [static|bert]             download the embedding model
//!   engram rpc '{"op": ...}'                     send one raw request to the daemon
//!   engram serve --dir D --project P             (the daemon; started on demand)

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde_json::{json, Value as Json};

use engram::{daemon, embed, hook, install, mcp, paths, transcript};

#[derive(Parser)]
#[command(name = "engram", version, about = "Automatic long-term context for Claude Code, stored in Strata")]
struct Cli {
    /// Project directory (default: $CLAUDE_PROJECT_DIR or the current directory).
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install hooks and the MCP server into the project's Claude Code settings.
    Install {
        /// Write hooks to .claude/settings.json (committed) instead of settings.local.json.
        #[arg(long)]
        shared: bool,
        /// Path of the engram binary to call (default: this one).
        #[arg(long)]
        exe: Option<String>,
    },
    /// Handle a Claude Code hook event (payload on stdin).
    Hook { event: String },
    /// Run the MCP server on stdio.
    Mcp,
    /// Run the daemon in the foreground.
    Serve {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Search the project's memory.
    Search {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
    },
    /// Run read-only SQL over the memory table.
    Sql { sql: String },
    /// Store a decision, preference, fact, todo or note.
    Remember { kind: String, text: String },
    /// Counts and sizes.
    Stats,
    /// Print the session-start briefing.
    Briefing,
    /// Write down the decisions, rules, requirements and facts of every
    /// stored turn (for a store recorded before distillation existed).
    Distill,
    /// Stop the daemon.
    Stop,
    /// Send one raw JSON request to the daemon and print its JSON response
    /// (the demo inspector's transport).
    Rpc {
        request: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Back-fill records from a Claude Code transcript.
    IngestTranscript { path: PathBuf },
    /// Download an embedding model (static: potion-retrieval-32M; bert: bge-small-en-v1.5).
    FetchModel {
        #[arg(default_value = "static")]
        which: String,
    },
}

fn send(project: &std::path::Path, req: Json, secs: u64) -> Json {
    let dir = paths::store_dir(project);
    match daemon::request(&dir, &project.to_string_lossy(), &req, Duration::from_secs(secs)) {
        Ok(r) => {
            if let Some(e) = r.get("error") {
                eprintln!("engram: {}", e.as_str().unwrap_or("error"));
                std::process::exit(1);
            }
            r
        }
        Err(e) => {
            eprintln!("engram: {e}");
            std::process::exit(1);
        }
    }
}

fn text(r: &Json) {
    println!("{}", r.get("text").and_then(|t| t.as_str()).unwrap_or(""));
}

fn main() {
    let cli = Cli::parse();
    let project = cli.project.clone().unwrap_or_else(|| paths::project_dir(None));
    let project = project.canonicalize().unwrap_or(project);
    match cli.cmd {
        Cmd::Hook { event } => std::process::exit(hook::run(&event)),
        Cmd::Mcp => std::process::exit(mcp::run()),
        Cmd::Serve { dir } => {
            let dir = dir.unwrap_or_else(|| paths::store_dir(&project));
            if let Err(e) = daemon::serve(&dir, &project.to_string_lossy()) {
                eprintln!("engram serve: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Install { shared, exe } => {
            let exe = exe.unwrap_or_else(|| std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "engram".into()));
            match install::install(&project, &exe, shared) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("engram install: {e}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Search { query, limit, kinds } => text(&send(&project, json!({"op": "search", "query": query, "limit": limit, "kinds": kinds, "include_current": true}), 30)),
        Cmd::Sql { sql } => text(&send(&project, json!({"op": "sql", "sql": sql}), 30)),
        Cmd::Remember { kind, text: t } => text(&send(&project, json!({"op": "remember", "kind": kind, "text": t}), 30)),
        Cmd::Stats => println!("{}", serde_json::to_string_pretty(&send(&project, json!({"op": "stats"}), 30)).unwrap()),
        Cmd::Briefing => text(&send(&project, json!({"op": "briefing", "session": ""}), 30)),
        Cmd::Distill => println!("{}", send(&project, json!({"op": "distill_backfill"}), 6 * 3600)),
        Cmd::Rpc { request, timeout } => {
            let req: Json = serde_json::from_str(&request).unwrap_or_else(|e| {
                eprintln!("engram: request is not JSON: {e}");
                std::process::exit(2)
            });
            let dir = paths::store_dir(&project);
            match daemon::request(&dir, &project.to_string_lossy(), &req, Duration::from_secs(timeout)) {
                Ok(r) => println!("{r}"),
                Err(e) => {
                    println!("{}", json!({"error": e.to_string()}));
                    std::process::exit(1);
                }
            }
        }
        Cmd::Stop => {
            send(&project, json!({"op": "shutdown"}), 10);
        }
        Cmd::IngestTranscript { path } => {
            let events = transcript::read(&path).unwrap_or_else(|e| {
                eprintln!("engram: {e}");
                std::process::exit(1)
            });
            let batches = transcript::drafts(&events, &project.to_string_lossy());
            let mut n = 0;
            for b in &batches {
                let r = send(&project, json!({"op": "append", "session": b.session, "turn": b.turn, "ts": b.ts, "drafts": b.drafts}), 120);
                n += r.get("stored").and_then(|v| v.as_u64()).unwrap_or(0);
            }
            println!("ingested {} events as {n} records from {}", events.len(), path.display());
        }
        Cmd::FetchModel { which } => {
            let (repo, name, files): (&str, &str, &[&str]) = match which.as_str() {
                "bert" => ("BAAI/bge-small-en-v1.5", "bge-small-en-v1.5", &["config.json", "tokenizer.json", "model.safetensors"]),
                _ => ("minishlab/potion-retrieval-32M", "potion-retrieval-32M", &["config.json", "tokenizer.json", "model.safetensors"]),
            };
            let dir = embed::models_dir().join(name);
            std::fs::create_dir_all(&dir).expect("create model dir");
            for f in files {
                let url = format!("https://huggingface.co/{repo}/resolve/main/{f}");
                let ok = std::process::Command::new("curl").args(["-fsSL", "-o"]).arg(dir.join(f)).arg(&url).status().map_or(false, |s| s.success());
                if !ok {
                    eprintln!("engram: failed to download {url}");
                    std::process::exit(1);
                }
            }
            println!("downloaded {repo} to {}", dir.display());
        }
    }
}
