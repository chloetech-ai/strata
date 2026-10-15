//! `engram doctor`: checks that Engram is wired into Claude Code on this
//! machine and that the project's daemon answers, and says how to fix what
//! is missing.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::{daemon, embed, paths};

struct Report {
    failed: usize,
}

impl Report {
    fn ok(&mut self, what: &str, detail: impl AsRef<str>) {
        println!("  ok    {what}: {}", detail.as_ref());
    }
    fn warn(&mut self, what: &str, detail: impl AsRef<str>) {
        println!("  warn  {what}: {}", detail.as_ref());
    }
    fn fail(&mut self, what: &str, detail: impl AsRef<str>) {
        self.failed += 1;
        println!("  FAIL  {what}: {}", detail.as_ref());
    }
}

fn read_json(path: &Path) -> Json {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null)
}

/// The commands of the Engram hooks in a settings file, by event.
fn engram_hooks(settings: &Json) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(h) = settings.get("hooks").and_then(|h| h.as_object()) {
        for (event, list) in h {
            for entry in list.as_array().into_iter().flatten() {
                for hk in entry.get("hooks").and_then(|v| v.as_array()).into_iter().flatten() {
                    let cmd = hk.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    if cmd.contains("engram") && cmd.contains(" hook ") {
                        out.push((event.clone(), cmd.to_string()));
                    }
                }
            }
        }
    }
    out
}

/// Checks the hooks in one settings file; returns whether there were any.
fn check_hooks(r: &mut Report, what: &str, path: &Path) -> bool {
    let hooks = engram_hooks(&read_json(path));
    if hooks.is_empty() {
        return false;
    }
    let events: Vec<&str> = hooks.iter().map(|(e, _)| e.as_str()).collect();
    let exe = hooks[0].1.split(" hook").next().unwrap_or("").trim().to_string();
    if !Path::new(&exe).is_file() {
        r.fail(what, format!("{} calls {exe}, which does not exist; run `engram install` again from the binary you want the hooks to use", path.display()));
    } else if events.len() < 9 {
        r.warn(what, format!("{} has Engram hooks for {} events only ({}); run `engram install` again", path.display(), events.len(), events.join(", ")));
    } else {
        r.ok(what, format!("{} ({} events, {exe})", path.display(), events.len()));
    }
    true
}

fn command_output(cmd: &str, args: &[&str]) -> Option<(bool, String)> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    let text = if out.stdout.is_empty() { &out.stderr } else { &out.stdout };
    Some((out.status.success(), String::from_utf8_lossy(text).trim().to_string()))
}

/// The last distillation the store's daemon logged, if any.
fn last_distill(store: &Path) -> Option<Json> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(store.join("events.log")).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(256 * 1024))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    buf.split(|&b| b == b'\n').rev().filter_map(|l| serde_json::from_slice::<Json>(l).ok()).find(|e| e["event"] == "distill")
}

pub fn run(project: &Path) -> i32 {
    let mut r = Report { failed: 0 };
    println!("Engram {} for {}", env!("CARGO_PKG_VERSION"), project.display());

    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    r.ok("binary", &exe);
    if std::env::var("ENGRAM_DISABLE").is_ok_and(|v| v == "1") {
        r.warn("ENGRAM_DISABLE", "is 1 in this shell: hooks started from here record nothing");
    }

    match command_output("claude", &["--version"]) {
        Some((true, v)) => r.ok("Claude Code CLI", v),
        _ => r.fail("Claude Code CLI", "`claude` is not on PATH; install Claude Code (https://claude.com/claude-code)"),
    }

    // Where the hooks come from: the user settings, the project, or both.
    let user_settings = paths::user_settings();
    let user = check_hooks(&mut r, "user hooks", &user_settings);
    let mut proj = false;
    for f in ["settings.local.json", "settings.json"] {
        let p = project.join(".claude").join(f);
        if p.canonicalize().ok() != user_settings.canonicalize().ok() {
            proj |= check_hooks(&mut r, "project hooks", &p);
        }
    }
    if !user && !proj {
        r.fail("hooks", format!("none in {} or {}/.claude; run `engram install --user` (every session) or `engram install` here", user_settings.display(), project.display()));
    } else if user && proj {
        r.ok("hooks", "the project's own install handles this project; the user-level hooks step aside here");
    }

    // The MCP server: user scope (claude mcp) or the project's .mcp.json.
    let mcp_json = read_json(&project.join(".mcp.json"));
    if mcp_json.pointer("/mcpServers/engram").is_some() {
        r.ok("MCP server", format!("{}/.mcp.json", project.display()));
    } else {
        // User-scope servers live in ~/.claude.json (`claude mcp get` would
        // start the server, and with it a store for this directory).
        let cfg_file = std::env::var("CLAUDE_CONFIG_DIR")
            .ok()
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(&d).join(".claude.json"))
            .unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap_or_default()).join(".claude.json"));
        match read_json(&cfg_file).pointer("/mcpServers/engram") {
            Some(s) => {
                let cmd = s.get("command").and_then(|c| c.as_str()).unwrap_or("");
                if Path::new(cmd).is_file() {
                    r.ok("MCP server", format!("user scope in {} ({cmd} mcp)", cfg_file.display()))
                } else {
                    r.fail("MCP server", format!("user scope in {} calls {cmd}, which does not exist; run `engram install --user` again", cfg_file.display()))
                }
            }
            None if user || proj => r.fail("MCP server", "not registered; run `engram install --user` again, or `claude mcp add-json engram '{\"type\":\"stdio\",\"command\":\"<engram path>\",\"args\":[\"mcp\"]}' --scope user`"),
            None => r.warn("MCP server", "not registered (Engram is not installed)"),
        }
    }

    // The embedding model the store will use.
    let store = paths::store_dir(project);
    let cfg = read_json(&store.join("config.json"));
    let spec = cfg.get("embedder").and_then(|v| v.as_str()).map(str::to_string).or_else(|| std::env::var("ENGRAM_EMBEDDER").ok()).unwrap_or_else(|| "static".into());
    let model_dir = embed::models_dir().join("potion-retrieval-32M");
    if spec == "static" && !model_dir.join("model.safetensors").is_file() {
        r.fail("embedding model", format!("{} is missing; run `engram fetch-model`", model_dir.display()));
    } else if spec == "static" {
        r.ok("embedding model", model_dir.display().to_string());
    } else {
        r.ok("embedding model", format!("embedder {spec}"));
    }

    // Distillation: which model, from where.
    let settings_env = |p: &Path| read_json(p).pointer("/env/ENGRAM_DISTILL_MODEL").and_then(|v| v.as_str()).map(str::to_string);
    let distill = std::env::var("ENGRAM_DISTILL_MODEL").ok().filter(|m| !m.is_empty()).map(|m| (m, "environment".to_string()))
        .or_else(|| settings_env(&project.join(".claude/settings.local.json")).map(|m| (m, "project settings".into())))
        .or_else(|| settings_env(&user_settings).map(|m| (m, "user settings".into())))
        .or_else(|| cfg.pointer("/distill/model").and_then(|v| v.as_str()).filter(|m| !m.is_empty()).map(|m| (m.to_string(), "store config.json".into())));
    if cfg.pointer("/distill/enabled") == Some(&json!(false)) || std::env::var("ENGRAM_DISTILL").is_ok_and(|v| v == "0") {
        r.warn("distillation", "off: no decisions, rules or planned work are written down");
    } else {
        match distill {
            Some((m, from)) => r.ok("distillation", format!("model {m} (from the {from})")),
            None => r.warn("distillation", "uses the Claude Code CLI's default model; a small one is enough (`engram install --user --distill-model haiku`)"),
        }
        // The distiller runs `claude -p`, which needs the CLI's own login
        // (an interactive session can work while the CLI's has expired).
        if let Some((_, out)) = command_output("claude", &["auth", "status"]) {
            if let Ok(st) = serde_json::from_str::<Json>(&out) {
                match st.get("loggedIn").and_then(|v| v.as_bool()) {
                    Some(false) => r.fail("Claude Code login", "the `claude` CLI is logged out, so every distillation fails; run `claude auth login`"),
                    Some(true) => r.ok("Claude Code login", "logged in"),
                    None => {}
                }
            }
        }
        if let Some(e) = last_distill(&store) {
            let when = e["ts"].as_i64().map(|t| format!("{} min ago", (crate::now() - t) / 60)).unwrap_or_default();
            match e.get("error").and_then(|v| v.as_str()) {
                Some(err) => r.warn("last distillation", format!("failed {when}: {err}")),
                None => r.ok("last distillation", format!("{when}, {} notes kept", e["kept"])),
            }
        }
    }

    // The daemon: the project's, when it has a store; otherwise one on a
    // scratch store, so that checking does not create a store here.
    let t0 = Instant::now();
    if store.join("memory.strata").exists() {
        match daemon::request(&store, &project.to_string_lossy(), &json!({"op": "stats"}), Duration::from_secs(30)) {
            Ok(s) if s.get("error").is_none() => r.ok(
                "store",
                format!("{} ({} records, {} sessions; daemon answered in {} ms)", store.display(), s["records"], s["sessions"], t0.elapsed().as_millis()),
            ),
            Ok(s) => r.fail("store", format!("{}: {}", store.display(), s["error"])),
            Err(e) => r.fail("store", format!("{}: {e}; see {}", store.display(), store.join("daemon.log").display())),
        }
    } else {
        let scratch = std::env::temp_dir().join(format!("engram-doctor-{}", std::process::id()));
        let res = daemon::request(&scratch, &project.to_string_lossy(), &json!({"op": "stats"}), Duration::from_secs(30));
        let log = std::fs::read_to_string(scratch.join("daemon.log")).unwrap_or_default();
        let _ = daemon::request(&scratch, "", &json!({"op": "shutdown"}), Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(&scratch);
        match res {
            Ok(s) if s.get("error").is_none() => r.ok(
                "daemon",
                format!("a scratch store opened and answered in {} ms; this project's store will be {} once a session runs here", t0.elapsed().as_millis(), store.display()),
            ),
            Ok(s) => r.fail("daemon", s["error"].to_string()),
            Err(e) => r.fail("daemon", format!("{e}: {}", log.trim())),
        }
    }

    if r.failed == 0 {
        println!("Engram is ready. Start `claude` in a project; `engram ui` shows what it stores.");
        0
    } else {
        println!("{} check{} failed.", r.failed, if r.failed == 1 { "" } else { "s" });
        1
    }
}
