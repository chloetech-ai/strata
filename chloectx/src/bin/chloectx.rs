//! chloectx: automatic long-term context for Claude Code, stored in ChloeDB.
//! (`engram`, its name before the rename, is a link to it for a release.)
//!
//! `chloectx --help` lists the everyday commands; the others are hidden
//! from it and named in [`MORE`], which a test keeps complete.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde_json::{json, Value as Json};

use chloectx::{daemon, doctor, embed, hook, install, mcp, paths, transcript, ui};

/// The commands `--help` does not list, by what they are for.
const MORE: &str = "\
Once installed, ChloeCtx works on its own: start Claude Code in a project and work
as usual; it records each session, recalls what bears on each prompt and writes
down decisions and rules. `chloectx doctor` says whether that is happening.

More commands (`chloectx help <command>` explains each):
  look into memory   blame, receipts, receipt, snapshot, sql, stats, briefing
  maintain a store   forget, move, redact, distill, consolidate, backup, fetch-model, ingest-transcript
  share memory       sync, pack, mount, app
  run by Claude Code (not by hand): hook, mcp, serve, rpc";

#[derive(Parser)]
#[command(name = "chloectx", version = env!("CHLOECTX_VERSION_LINE"), about = "Automatic long-term context for Claude Code, stored in ChloeDB", after_help = MORE)]
struct Cli {
    /// Project directory (default: $CLAUDE_PROJECT_DIR or the current directory).
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wire ChloeCtx into Claude Code (a project's, or with --user every session)
    #[command(display_order = 1)]
    Install {
        /// Write hooks to .claude/settings.json (committed) instead of settings.local.json.
        #[arg(long)]
        shared: bool,
        /// Install for every Claude Code session: hooks in ~/.claude/settings.json,
        /// the MCP server at user scope. Each project still gets its own store.
        #[arg(long, conflicts_with = "shared")]
        user: bool,
        /// Path of the chloectx binary to call (default: this one).
        #[arg(long)]
        exe: Option<String>,
        /// Model to distill turns with (sets CHLOECTX_DISTILL_MODEL in the settings' env).
        #[arg(long)]
        distill_model: Option<String>,
    },
    /// Remove what `install` wrote (stores are kept)
    ///
    /// The project's install, or with --user the user-level one.
    #[command(display_order = 2)]
    Uninstall {
        #[arg(long)]
        user: bool,
    },
    /// Handle a Claude Code hook event (payload on stdin).
    #[command(hide = true)]
    Hook {
        /// Called from the user settings: step aside in a project with its own install.
        #[arg(long)]
        user: bool,
        event: String,
    },
    /// Run the MCP server on stdio.
    #[command(hide = true)]
    Mcp,
    /// Run the daemon in the foreground.
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Search the project's memory.
    #[command(display_order = 7)]
    Search {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
    },
    /// Answer a question about earlier work
    ///
    /// The rules, notes, replies and messages that bear on it, found by
    /// search alone and packed for it.
    #[command(display_order = 6)]
    Ask {
        question: String,
        /// A model plans several searches and a second call picks the records.
        #[arg(long)]
        deep: bool,
        /// With --deep: model for planning and ranking (default: the configured one).
        #[arg(long)]
        model: Option<String>,
        /// Print the JSON result (picked ids, answer, confidence, cost).
        #[arg(long)]
        json: bool,
    },
    /// Run read-only SQL over the memory table.
    #[command(hide = true)]
    Sql { sql: String },
    /// Store a decision, preference, fact, todo or note.
    #[command(display_order = 8)]
    Remember { kind: String, text: String },
    /// Delete records from memory for good: this project's by id, or with
    /// --everywhere what every store holds of a fact
    ///
    /// The ids are the ones `chloectx search` and the inspector show
    /// (`session:seq`). Each record is deleted, scrubbed from the receipts
    /// that showed it, and the automatic snapshots that hold it are dropped;
    /// the other records of its turn stay. It cannot be undone.
    ///
    /// With --everywhere, every store `chloectx stores` lists is searched
    /// for records whose text or context holds the words given (whatever
    /// their case; 3 characters or more), or, when each word is a record id,
    /// for those records. The matches are listed by store, and forgotten
    /// once you say yes (or with --yes; --dry-run only lists them). A store
    /// synced to a server gets them back at its next pull while the server
    /// holds them: delete them there too.
    #[command(hide = true)]
    Forget {
        /// Record ids, such as a1b2c3d4:42; with --everywhere, the text to look for, or ids.
        #[arg(required = true)]
        ids: Vec<String>,
        /// In every store on this machine, not only this project's.
        #[arg(long)]
        everywhere: bool,
        /// With --everywhere: forget the matches without asking.
        #[arg(long, short, requires = "everywhere")]
        yes: bool,
        /// With --everywhere: only list the matches.
        #[arg(long, requires = "everywhere")]
        dry_run: bool,
    },
    /// The edit behind a line of code (`path:line`) or a file: its session,
    /// prompt and tool calls, the notes written from its turn, the records
    /// memory had recalled into the agent's context before it (with their
    /// scores and what became of them), and that recall replayed on the
    /// snapshot kept at the time and on today's store.
    #[command(hide = true)]
    Blame {
        target: String,
        /// Do not replay the recall.
        #[arg(long)]
        no_replay: bool,
        /// Print the JSON result.
        #[arg(long)]
        json: bool,
    },
    /// The newest receipts: one for every recall and memory search. With
    /// `on` or `off`, turns receipts and automatic snapshots on or off for
    /// this project's store (in its config.json, which wins over
    /// $CHLOECTX_RECEIPTS).
    #[command(hide = true)]
    Receipts {
        #[arg(value_parser = ["on", "off"])]
        switch: Option<String>,
        /// With `off`: also remove the receipts and automatic snapshots kept so far.
        #[arg(long, requires = "switch")]
        purge: bool,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One receipt: every candidate's scores, tokens and decision, and the
    /// recall replayed on the snapshot kept at its commit and on today's store.
    #[command(hide = true)]
    Receipt {
        id: String,
        #[arg(long)]
        no_replay: bool,
        /// Also print the text the agent was shown.
        #[arg(long)]
        text: bool,
        #[arg(long)]
        json: bool,
    },
    /// Snapshots of the store kept by name, which receipts replay on
    /// (others are kept after recalls and pruned by age).
    #[command(hide = true)]
    Snapshot {
        #[command(subcommand)]
        what: SnapCmd,
    },
    /// Counts and sizes.
    #[command(hide = true)]
    Stats,
    /// Copy the project's store to a new directory while ChloeCtx runs: a
    /// consistent copy of the database, the session state and the event
    /// log. To restore, run `chloectx stop` and copy the files back.
    #[command(hide = true)]
    Backup { dest: PathBuf },
    /// Print the session-start briefing.
    #[command(hide = true)]
    Briefing,
    /// Write down the decisions, rules, requirements and facts of every
    /// stored turn (for a store recorded before distillation existed).
    #[command(hide = true)]
    Distill,
    /// Merge related notes now (it also runs on its own every few turns).
    #[command(hide = true)]
    Consolidate,
    /// Redact the secrets a store recorded before redaction knew their shape:
    /// rewrites the project's records (with --all, every store's), session
    /// state and event log, compacts the store, and replaces the records on
    /// the sync's server. Prints how many changed, never the secrets.
    #[command(hide = true)]
    Redact {
        #[arg(long)]
        all: bool,
        /// Count what would change, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop the project's daemon (with --all, every one); hooks start it again
    #[command(display_order = 11)]
    Stop {
        #[arg(long)]
        all: bool,
    },
    /// Check that ChloeCtx works here, and say how to fix what does not
    #[command(display_order = 3)]
    Doctor,
    /// Install the newest version from the checkout ChloeCtx was built from
    ///
    /// Pulls the checkout (fast forward only), builds and installs it over
    /// this binary, stops the old daemons, wires the user-level hooks again
    /// (a version may add some), moves the store folder from its name
    /// before the rename, and runs doctor.
    #[command(display_order = 4)]
    Upgrade {
        /// Only say whether a newer version is there.
        #[arg(long)]
        check: bool,
        /// Build the checkout as it is, without `git pull`.
        #[arg(long)]
        no_pull: bool,
        /// Build and install even when nothing changed.
        #[arg(long)]
        force: bool,
    },
    /// Attach the memory of a project that moved to its new folder
    ///
    /// Stores are kept by the project folder's path, so a project that is
    /// moved or renamed starts with an empty memory. This makes the store of
    /// OLD the store of NEW (which must have none of its own yet).
    #[command(hide = true)]
    Move {
        /// Where the project was.
        old: PathBuf,
        /// Where it is now.
        new: PathBuf,
    },
    /// List every store on this machine: its project, size and last use
    #[command(display_order = 9)]
    Stores {
        #[arg(long)]
        json: bool,
    },
    /// Remove the stores nothing will use again
    ///
    /// Those whose project folder is gone, or is a temporary folder unused
    /// for a day, or that name no project and were unused for a month.
    /// Lists them and asks first (or pass --yes).
    #[command(display_order = 10)]
    Prune {
        /// Also remove stores not used for this many days.
        #[arg(long)]
        older_than: Option<f64>,
        /// Remove them without asking.
        #[arg(long, short)]
        yes: bool,
        /// Only list what would be removed.
        #[arg(long)]
        dry_run: bool,
    },
    /// Browse what ChloeCtx stored, in the inspector (a local web page)
    #[command(display_order = 5)]
    Ui {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port (the next free one up to 9 above it is used if it is taken).
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Directory holding projects/<store>/ (default: $CHLOECTX_HOME, or ~/.local/share/chloectx;
        /// ~/.local/share/engram on a machine that has only that one).
        #[arg(long)]
        home: Option<PathBuf>,
        /// Do not open a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Serve ChloeCtx as an app of a ChloeDB cluster (`chloedb-server --app
    /// chloectx=URL`): memory spaces any agent writes to and asks for
    /// context, kept in the cluster's tables, and Seen before, the cluster's
    /// firing alerts beside the past incidents they look like.
    #[command(hide = true)]
    App {
        /// The chloedb-server (a coordinator) the spaces live in.
        #[arg(long, env = "CHLOEDB_URL", default_value = "http://127.0.0.1:7700")]
        server: String,
        /// Its key, for the app's reads and writes ($CHLOEDB_KEY).
        #[arg(long, env = "CHLOEDB_KEY", hide_env_values = true)]
        key: Option<String>,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 7760)]
        port: u16,
        /// The token callers must send as `Authorization: Bearer` (chloedb-server
        /// sends its CHLOEDB_APP_TOKEN); none: any caller ($CHLOECTX_APP_TOKEN).
        #[arg(long, env = "CHLOECTX_APP_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Serve any caller with no token on an address other than loopback
        /// (without it, the app refuses to start that way).
        #[arg(long)]
        insecure: bool,
        /// The embedder (static, or hash:<dim> for tests; $CHLOECTX_EMBEDDER).
        #[arg(long, env = "CHLOECTX_EMBEDDER", default_value = "static")]
        embedder: String,
        /// Also show this machine's Claude Code stores in the command
        /// center (Receipts and Blame pages; $CHLOECTX_APP_LOCAL=1). Their
        /// records are transcripts: turn it on only where the command
        /// center's users may read them.
        #[arg(long, env = "CHLOECTX_APP_LOCAL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false)]
        local_stores: bool,
    },
    /// Cut a capsule from the project's memory: a read-only ChloeDB file of
    /// the selected records (default: distilled notes, entities and the
    /// conversation, no raw tool output), redacted again, with a manifest.
    /// Another agent mounts it with `chloectx mount`.
    #[command(hide = true)]
    Pack {
        /// The capsule's directory (must not exist).
        out: PathBuf,
        /// Record kinds to include (default: decision, preference, fact, todo,
        /// note, entity, prompt, answer, episode, summary, error; `all` for every kind).
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        /// Only records from the last N days.
        #[arg(long)]
        since_days: Option<f64>,
        /// At most this many records (the oldest first).
        #[arg(long, default_value_t = 200_000)]
        max_records: usize,
        /// The capsule's title (default: the project's folder name).
        #[arg(long)]
        title: Option<String>,
        /// Print the manifest as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Serve capsules to Claude Code as an MCP server on stdio
    /// (capsule_ask, capsule_search, capsule_timeline, capsule_sql).
    #[command(hide = true)]
    Mount {
        /// Capsule directories.
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
        /// Append one JSON line per tool call (and the open) to this file ($CHLOECTX_CAPSULE_LOG).
        #[arg(long)]
        log: Option<PathBuf>,
        /// Embedder spec, instead of the one each manifest names.
        #[arg(long)]
        embedder: Option<String>,
    },
    /// Send one raw JSON request to the daemon and print its JSON response
    /// (the demo inspector's transport).
    #[command(hide = true)]
    Rpc {
        request: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Back-fill records from a Claude Code transcript.
    #[command(hide = true)]
    IngestTranscript { path: PathBuf },
    /// Copy the project's memory to a ChloeDB server or cluster, and back.
    #[command(hide = true)]
    Sync {
        #[command(subcommand)]
        what: SyncCmd,
    },
    /// Download an embedding model (static: potion-retrieval-32M; bert: bge-small-en-v1.5).
    #[command(hide = true)]
    FetchModel {
        #[arg(default_value = "static")]
        which: String,
    },
}

#[derive(Subcommand)]
enum SnapCmd {
    /// Keep a snapshot of the store now, under NAME, until it is dropped.
    Keep { name: String },
    /// The kept snapshots.
    List,
    /// Drop a kept snapshot.
    Drop { name: String },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Name the server (e.g. https://chloedb.example.com) and create its table.
    Setup {
        #[arg(long)]
        url: String,
        /// Read the server's key (an API key or token) from stdin, kept
        /// owner-only in ChloeCtx's home, never on the command line.
        #[arg(long, conflicts_with = "token_env")]
        token_stdin: bool,
        /// Read the key from this environment variable whenever the daemon
        /// syncs (one a cloud environment sets, say); no file holds it.
        #[arg(long, default_value = "")]
        token_env: String,
        /// A repository renamed, as owner/old=owner/new (repeatable): a
        /// checkout under either name syncs as the new one.
        #[arg(long = "rename")]
        renames: Vec<String>,
        /// Do not create the table (it exists, or the key cannot: it needs the admin scope).
        #[arg(long)]
        no_create: bool,
    },
    /// What was sent and received, and what the server holds for this project.
    Status,
    /// Send what the store recorded since the last push.
    Push,
    /// Push, then store the project's records the server holds and the store lacks.
    Pull,
    /// The records pulled from other machines whose rules and decisions are
    /// kept out of the briefing until accepted, by machine; with --origin
    /// (repeatable) or --all, accept those machines' records, now and on
    /// later pulls.
    Accept {
        #[arg(long = "origin")]
        origins: Vec<String>,
        #[arg(long)]
        all: bool,
    },
}

fn send(project: &std::path::Path, req: Json, secs: u64) -> Json {
    let dir = paths::store_dir(project);
    match daemon::request(&dir, &project.to_string_lossy(), &req, Duration::from_secs(secs)) {
        Ok(r) => {
            if let Some(e) = r.get("error") {
                eprintln!("chloectx: {}", e.as_str().unwrap_or("error"));
                std::process::exit(1);
            }
            r
        }
        Err(e) => {
            eprintln!("chloectx: {e}");
            std::process::exit(1);
        }
    }
}

/// `chloectx redact` for one store: the daemon rewrites the records, then
/// (when it changed any) the file is replaced by a copy without the pages
/// their old versions were on. What it did, without the secrets.
fn redact_store(dir: &std::path::Path, project: &str, dry_run: bool) -> Result<String, String> {
    let r = daemon::request(dir, project, &json!({"op": "redact", "dry_run": dry_run}), Duration::from_secs(6 * 3600)).map_err(|e| e.to_string())?;
    if let Some(e) = r.get("error") {
        return Err(e.as_str().unwrap_or("error").to_string());
    }
    let mut out = r["text"].as_str().unwrap_or("").to_string();
    if !dry_run && r["changed"].as_u64().unwrap_or(0) > 0 {
        match daemon::rewrite_file(dir, Duration::from_secs(600)) {
            Ok(f) => out += &format!(" The store's file was rewritten without the old versions ({} to {} bytes).", f["bytes_before"], f["bytes_after"]),
            Err(e) => return Err(format!("{out} Rewriting the store's file failed, so the old versions may stay in its free pages until they are reused: {e}")),
        }
    }
    Ok(out)
}

fn text(r: &Json) {
    println!("{}", r.get("text").and_then(|t| t.as_str()).unwrap_or(""));
}

fn main() {
    chloectx::env::adopt_old_names();
    let cli = Cli::parse();
    let project = cli.project.clone().unwrap_or_else(|| paths::project_dir(None));
    let project = project.canonicalize().unwrap_or(project);
    match cli.cmd {
        Cmd::Hook { event, user } => std::process::exit(hook::run(&event, user)),
        Cmd::Mcp => std::process::exit(mcp::run()),
        Cmd::Serve { dir } => {
            let dir = dir.unwrap_or_else(|| paths::store_dir(&project));
            if let Err(e) = daemon::serve(&dir, &project.to_string_lossy()) {
                eprintln!("chloectx serve: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Install { shared, user, exe, distill_model } => {
            let exe = exe.unwrap_or_else(|| std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "chloectx".into()));
            let dm = distill_model.as_deref();
            match if user { install::install_user(&exe, dm) } else { install::install(&project, &exe, shared, dm) } {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("chloectx install: {e}");
                    std::process::exit(1);
                }
            }
        }
        Cmd::Uninstall { user } => match if user { install::uninstall_user() } else { install::uninstall(&project) } {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("chloectx uninstall: {e}");
                std::process::exit(1);
            }
        },
        Cmd::Search { query, limit, kinds } => text(&send(&project, json!({"op": "search", "query": query, "limit": limit, "kinds": kinds, "include_current": true}), 30)),
        Cmd::Ask { question, deep, model, json: as_json } => {
            let r = send(&project, json!({"op": "ask", "question": question, "deep": deep || model.is_some(), "model": model, "include_current": true}), 300);
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else {
                text(&r);
            }
        }
        Cmd::Sql { sql } => text(&send(&project, json!({"op": "sql", "sql": sql}), 30)),
        Cmd::Blame { target, no_replay, json: as_json } => {
            // Relative to where it is typed, not to the daemon's directory.
            let (path, line) = chloectx::blame::parse_target(&target);
            let abs = std::path::absolute(&path).unwrap_or_else(|_| path.clone().into());
            let target = match line {
                Some(n) => format!("{}:{n}", abs.display()),
                None => abs.display().to_string(),
            };
            let r = send(&project, json!({"op": "blame", "target": target, "replay": !no_replay}), 120);
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else {
                text(&r);
            }
        }
        Cmd::Receipts { switch: Some(s), purge, .. } => text(&send(&project, json!({"op": "receipts_switch", "on": s == "on", "purge": purge}), 600)),
        Cmd::Receipts { switch: None, session, limit, .. } => text(&send(&project, json!({"op": "receipts", "session": session, "limit": limit}), 30)),
        Cmd::Receipt { id, no_replay, text: show, json: as_json } => {
            let r = send(&project, json!({"op": "receipt", "id": id, "replay": !no_replay, "text": show}), 120);
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else {
                text(&r);
            }
        }
        Cmd::Snapshot { what } => {
            let req = match what {
                SnapCmd::Keep { name } => json!({"op": "snapshot", "what": "keep", "name": name}),
                SnapCmd::List => json!({"op": "snapshot", "what": "list"}),
                SnapCmd::Drop { name } => json!({"op": "snapshot", "what": "drop", "name": name}),
            };
            text(&send(&project, req, 600));
        }
        Cmd::Remember { kind, text: t } => text(&send(&project, json!({"op": "remember", "kind": kind, "text": t}), 30)),
        Cmd::Stats => println!("{}", serde_json::to_string_pretty(&send(&project, json!({"op": "stats"}), 30)).unwrap()),
        Cmd::Backup { dest } => {
            let dest = std::path::absolute(&dest).unwrap_or(dest);
            let r = send(&project, json!({"op": "backup", "dest": dest}), 3600);
            println!("{}: {} records, {} bytes, in {:.1} s", dest.display(), r["records"], r["bytes"], r["seconds"].as_f64().unwrap_or(0.0));
        }
        Cmd::Briefing => text(&send(&project, json!({"op": "briefing", "session": ""}), 30)),
        Cmd::Distill => println!("{}", send(&project, json!({"op": "distill_backfill"}), 6 * 3600)),
        Cmd::Consolidate => println!("{}", send(&project, json!({"op": "consolidate"}), 3600)["report"]),
        Cmd::Rpc { request, timeout } => {
            let req: Json = serde_json::from_str(&request).unwrap_or_else(|e| {
                eprintln!("chloectx: request is not JSON: {e}");
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
        Cmd::Sync { what: SyncCmd::Setup { url, token_stdin, token_env, renames, no_create } } => {
            let token = token_stdin.then(|| {
                let mut t = String::new();
                let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut t);
                t
            });
            let home = paths::home();
            if let Err(e) = chloectx::sync::setup(&home, &url, token.as_deref(), &token_env, &renames) {
                eprintln!("chloectx sync: {e}");
                std::process::exit(1);
            }
            let remote = chloectx::sync::remote(&home).expect("just set up");
            if !no_create {
                let key = chloectx::sync::project_key(&home, &project.to_string_lossy());
                if let Err(e) = remote.create_table(&key) {
                    eprintln!("chloectx sync: could not create the table {} on {}: {e}", chloectx::sync::table_for(&key), remote.url);
                    std::process::exit(1);
                }
            }
            // Not fatal: the key may only be in the environment of the
            // sessions to come (a cloud setup script runs before it is).
            let key = chloectx::sync::project_key(&home, &project.to_string_lossy());
            match remote.count(&key) {
                Ok(n) => println!("sync set up with {}; this project syncs as {key} ({n} records there)", remote.url),
                Err(e) => println!("sync set up with {}; this project syncs as {key}; the server does not answer yet ({e}): the daemon tries again when it starts", remote.url),
            }
            // A running daemon syncs once it starts again (on the next hook or tool).
            let dir = paths::store_dir(&project);
            if std::os::unix::net::UnixStream::connect(paths::StorePaths::new(&dir).socket).is_ok() {
                send(&project, json!({"op": "shutdown"}), 10);
            }
        }
        Cmd::Sync { what: SyncCmd::Accept { mut origins, all } } => {
            if all {
                origins = vec!["*".into()];
            }
            let r = send(&project, json!({"op": "sync", "what": "accept", "origins": origins}), 600);
            println!("{}", r["text"].as_str().unwrap_or(""));
        }
        Cmd::Sync { what } => {
            let w = match what {
                SyncCmd::Status => "status",
                SyncCmd::Push => "push",
                SyncCmd::Pull => "pull",
                SyncCmd::Setup { .. } | SyncCmd::Accept { .. } => unreachable!(),
            };
            println!("{}", serde_json::to_string_pretty(&send(&project, json!({"op": "sync", "what": w}), 600)).unwrap());
        }
        Cmd::Doctor => std::process::exit(doctor::run(&project)),
        Cmd::Upgrade { check, no_pull, force } => std::process::exit(chloectx::version::upgrade(&project, &chloectx::version::UpgradeOptions { check, no_pull, force })),
        Cmd::Move { old, new } => match chloectx::stores::relink(&paths::home(), &old, &new) {
            Ok(t) => println!("{t}"),
            Err(e) => {
                eprintln!("chloectx move: {e}");
                std::process::exit(1);
            }
        },
        Cmd::Forget { ids, everywhere: true, yes, dry_run } => std::process::exit(chloectx::stores::forget_everywhere(&paths::home(), &ids, &chloectx::stores::ForgetOptions { yes, dry_run })),
        Cmd::Forget { ids, .. } => {
            // Only a store that exists: forgetting never creates one.
            let dir = paths::store_dir(&project);
            if !dir.join("memory.strata").exists() {
                eprintln!("chloectx forget: {} has no store ({}); run it in the project's folder, or pass --project", project.display(), dir.display());
                std::process::exit(1);
            }
            let mut missing = 0;
            for id in &ids {
                let r = send(&project, json!({"op": "forget", "id": id, "wait": true}), 120);
                let t = r["text"].as_str().unwrap_or("");
                println!("{t}");
                missing += usize::from(t.starts_with("No record"));
            }
            if missing > 0 {
                std::process::exit(1);
            }
        }
        Cmd::Stores { json: as_json } => std::process::exit(chloectx::stores::list(&paths::home(), as_json)),
        Cmd::Prune { older_than, yes, dry_run } => std::process::exit(chloectx::stores::prune(&paths::home(), &chloectx::stores::PruneOptions { older_than_days: older_than, yes, dry_run })),
        Cmd::Pack { out, kinds, since_days, max_records, title, json: as_json } => {
            let t0 = std::time::Instant::now();
            let out = std::path::absolute(&out).unwrap_or(out);
            let kinds: Vec<chloectx::record::Kind> = if kinds.iter().any(|k| k == "all") {
                chloectx::record::Kind::ALL.to_vec()
            } else if kinds.is_empty() {
                chloectx::capsule::DEFAULT_KINDS.to_vec()
            } else {
                kinds.iter().map(|k| chloectx::record::Kind::parse(k).unwrap_or_else(|| {
                    eprintln!("chloectx pack: unknown kind {k}");
                    std::process::exit(2)
                })).collect()
            };
            // A consistent copy of the store, which no daemon writes while
            // it is cut, in a directory only this user can open: it holds
            // the whole store, its event log and session state.
            let tmp = paths::private_temp_dir("chloectx-pack").unwrap_or_else(|e| {
                eprintln!("chloectx pack: a private temporary directory: {e}");
                std::process::exit(1)
            });
            let backup = tmp.join("backup");
            send(&project, json!({"op": "backup", "dest": backup}), 3600);
            let backup_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let spec = daemon::Config::load(&backup.join("config.json")).embedder;
            let fail = |e: chloectx::Error| -> ! {
                let _ = std::fs::remove_dir_all(&tmp);
                eprintln!("chloectx pack: {e}");
                std::process::exit(1)
            };
            let embedder: std::sync::Arc<dyn embed::Embedder> = std::sync::Arc::from(embed::open(&spec).unwrap_or_else(|e| fail(e)));
            let store = chloectx::store::Store::open(&backup.join("memory.strata"), embedder, chloectx::store::StoreOptions { sync: false, ..Default::default() }).unwrap_or_else(|e| fail(e));
            let opts = chloectx::capsule::PackOptions { kinds, since_ts: since_days.map(|d| chloectx::now() - (d * 86_400.0) as i64), max_records, title };
            let m = chloectx::capsule::pack(&store, &spec, &project.to_string_lossy(), &out, &opts).unwrap_or_else(|e| fail(e));
            drop(store);
            let _ = std::fs::remove_dir_all(&tmp);
            let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
            if as_json {
                let mut j = serde_json::to_value(&m).unwrap();
                j["backup_ms"] = json!(backup_ms);
                j["total_ms"] = json!(total_ms);
                j["dir"] = json!(out);
                println!("{}", serde_json::to_string_pretty(&j).unwrap());
            } else {
                println!("{}: {}", out.display(), chloectx::capsule::describe(&m));
                println!("{} bytes, sha256 {}, {} redacted, packed in {:.0} ms", m.bytes, m.sha256, m.redacted, total_ms);
            }
        }
        Cmd::Mount { dirs, log, embedder } => std::process::exit(chloectx::capsule::run_mcp(&dirs, log, embedder.as_deref())),
        Cmd::App { server, key, host, port, token, insecure, embedder, local_stores } => {
            let local = local_stores.then(paths::home);
            if let Err(e) = chloectx::app::run(chloectx::app::Options { host, port, server, key, token: token.filter(|t| !t.is_empty()), insecure, embedder, local }) {
                eprintln!("chloectx app: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Ui { host, port, home, no_open } => {
            let home = home.map(|h| h.canonicalize().unwrap_or(h)).unwrap_or_else(paths::home);
            if let Err(e) = ui::run(ui::Options { host, port, home, open: !no_open, project: project.clone() }) {
                eprintln!("chloectx ui: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Redact { all, dry_run } => {
            let stores: Vec<(PathBuf, String, String)> = if all {
                ui::list_stores(&paths::home()).into_iter().map(|s| (s.dir, s.project.clone().unwrap_or_default(), s.project.unwrap_or(s.id))).collect()
            } else {
                let p = project.to_string_lossy().into_owned();
                vec![(paths::store_dir(&project), p.clone(), p)]
            };
            let mut failed = 0;
            for (dir, proj, name) in stores {
                let prefix = if all { format!("{name}: ") } else { String::new() };
                // One store at a time, and a daemon started for it here is
                // stopped after it (CTX-8: `--all` left one per store).
                let was_running = std::os::unix::net::UnixStream::connect(paths::StorePaths::new(&dir).socket).is_ok();
                let res = redact_store(&dir, &proj, dry_run);
                if !was_running {
                    let _ = daemon::request_running(&dir, &json!({"op": "shutdown"}), Duration::from_secs(10));
                }
                match res {
                    Ok(t) => println!("{prefix}{t}"),
                    Err(e) => {
                        failed += 1;
                        eprintln!("chloectx redact: {prefix}{e}");
                    }
                }
            }
            if failed > 0 {
                std::process::exit(1);
            }
        }
        Cmd::Stop { all: false } => {
            // Only a daemon that runs: stopping never starts one, nor
            // creates a store (CTX-13).
            let dir = paths::store_dir(&project);
            match daemon::request_running(&dir, &json!({"op": "shutdown"}), Duration::from_secs(10)) {
                Ok(_) => println!("stopped the daemon of {}; it starts again when a hook or tool needs it", project.display()),
                Err(_) => println!("no daemon runs for {}", project.display()),
            }
        }
        Cmd::Stop { all: true } => {
            let mut n = 0;
            for s in ui::list_stores(&paths::home()) {
                let sock = paths::StorePaths::new(&s.dir).socket;
                if std::os::unix::net::UnixStream::connect(&sock).is_ok()
                    && daemon::request(&s.dir, s.project.as_deref().unwrap_or(""), &json!({"op": "shutdown"}), Duration::from_secs(10)).is_ok()
                {
                    n += 1;
                }
            }
            println!("stopped {n} daemon{}; each starts again when a hook or tool needs it", if n == 1 { "" } else { "s" });
        }
        Cmd::IngestTranscript { path } => {
            let events = transcript::read(&path).unwrap_or_else(|e| {
                eprintln!("chloectx: {e}");
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
            let &(_, repo, name, files) = embed::FETCHED.iter().find(|m| m.0 == which).unwrap_or(&embed::FETCHED[0]);
            let dir = embed::models_dir().join(name);
            match embed::fetch_model("https://huggingface.co", repo, &dir, files) {
                Ok(done) if done.iter().all(|f| matches!(f, embed::Fetched::Kept(_))) => println!("{repo} is already in {}, as pinned", dir.display()),
                Ok(_) => println!("downloaded {repo} to {}, each file checked against its pinned SHA-256", dir.display()),
                Err(e) => {
                    eprintln!("chloectx: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Every command is either in `--help` or named in [`MORE`].
    #[test]
    fn every_command_is_listed_somewhere() {
        let cmd = Cli::command();
        let mut visible = Vec::new();
        for c in cmd.get_subcommands() {
            let name = c.get_name();
            if c.is_hide_set() {
                assert!(MORE.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-')).any(|w| w == name), "`{name}` is hidden from --help and not named in MORE");
            } else {
                visible.push((c.get_display_order(), name.to_string()));
            }
        }
        visible.sort();
        let visible: Vec<String> = visible.into_iter().map(|v| v.1).collect();
        assert_eq!(visible, ["install", "uninstall", "doctor", "upgrade", "ui", "ask", "search", "remember", "stores", "prune", "stop"]);
    }
}
