//! `chloectx doctor`: checks that ChloeCtx is wired into Claude Code on this
//! machine and that the project's daemon answers, and says how to fix what
//! is missing.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::{daemon, embed, install, paths};

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

/// The commands of the ChloeCtx hooks in a settings file, by event.
fn own_hooks(settings: &Json) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(h) = settings.get("hooks").and_then(|h| h.as_object()) {
        for (event, list) in h {
            for entry in list.as_array().into_iter().flatten() {
                for hk in entry.get("hooks").and_then(|v| v.as_array()).into_iter().flatten() {
                    let cmd = hk.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    if install::hook_exe(cmd).is_some() {
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
    let hooks = own_hooks(&read_json(path));
    if hooks.is_empty() {
        return false;
    }
    let events: Vec<&str> = hooks.iter().map(|(e, _)| e.as_str()).collect();
    let exe = install::hook_exe(&hooks[0].1).unwrap_or_default();
    let old_name = Path::new(&exe).file_name().map_or(false, |n| !n.to_string_lossy().contains(install::SERVER));
    if !Path::new(&exe).is_file() {
        r.fail(what, format!("{} calls {exe}, which does not exist; run `chloectx install` again from the binary you want the hooks to use", path.display()));
    } else if let Err(e) = runs_in_a_shell(&hooks[0].1) {
        r.fail(what, format!("{} has the hook command `{}`, which a shell cannot run ({e}); run `chloectx install` again", path.display(), hooks[0].1));
    } else if old_name {
        r.warn(what, format!("{} calls {exe}, ChloeCtx's name before the rename; run `chloectx install` again to move to the new names", path.display()));
    } else if install::EVENTS.iter().any(|(e, _, _)| !events.contains(e)) {
        let missing: Vec<&str> = install::EVENTS.iter().map(|(e, _, _)| *e).filter(|e| !events.contains(e)).collect();
        r.warn(what, format!("{} has ChloeCtx hooks for {} events only, without {}; run `chloectx install` again", path.display(), events.len(), missing.join(", ")));
    } else {
        r.ok(what, format!("{} ({} events, {exe})", path.display(), events.len()));
    }
    true
}

/// Runs a hook command's program, as the command spells it, with a shell
/// (as Claude Code does) and `--version`: a path with a space that is not
/// quoted runs something else.
pub fn runs_in_a_shell(command: &str) -> std::result::Result<(), String> {
    let exe = install::hook_exe_text(command).ok_or("not a ChloeCtx hook command")?;
    let out = std::process::Command::new("sh").arg("-c").arg(format!("{exe} --version")).stdin(std::process::Stdio::null()).output().map_err(|e| format!("sh: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && (said.contains(install::SERVER) || said.contains(install::OLD_SERVER)) {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().next().unwrap_or("it failed").trim().to_string())
    }
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
    println!("ChloeCtx {} for {}", env!("CARGO_PKG_VERSION"), project.display());

    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    r.ok("binary", &exe);
    let home = paths::home();
    // Where this build stands against the checkout it was built from.
    // The fetch is bounded: a remote that never answers costs 3 s, and a
    // fetch that failed is said to have failed (CTX-4).
    if let Some(v) = crate::version::check(&home, Some(crate::version::DOCTOR_FETCH_TIMEOUT)) {
        if v.behind > 0 || v.fetch_error.is_some() {
            r.warn("version", v.line());
        } else {
            r.ok("version", v.line());
        }
    } else if crate::version::source_dir().is_none() {
        // A prebuilt binary: no checkout to compare with (CTX-5).
        r.ok("version", format!("{} (no checkout on this machine to compare with; to upgrade, run the installer again: {})", env!("CARGO_PKG_VERSION"), crate::version::RELEASE_INSTALL));
    }
    if home.file_name().map_or(false, |n| n == paths::OLD_HOME) {
        r.warn("stores", format!("{} is the folder's name before the rename; `chloectx upgrade` moves it to {}", home.display(), home.with_file_name(paths::HOME).display()));
    }
    if std::env::var("CHLOECTX_DISABLE").is_ok_and(|v| v == "1") {
        r.warn("CHLOECTX_DISABLE", "is 1 in this shell (or ENGRAM_DISABLE, its name before the rename): hooks started from here record nothing");
    }

    // The CLI as the daemon finds it too: its PATH is the one of whichever
    // process started it, so it also looks in the usual places (CTX-3).
    let claude = crate::llm::resolve_cli("claude");
    match claude.as_ref().ok().and_then(|p| command_output(&p.to_string_lossy(), &["--version"]).map(|o| (p, o))) {
        Some((p, (true, v))) => {
            let note = if crate::llm::on_path("claude").is_none() { "; not on this shell's PATH, found in a usual place" } else { "" };
            r.ok("Claude Code CLI", format!("{v} ({}{note})", p.display()))
        }
        Some((p, (false, out))) => r.fail("Claude Code CLI", format!("{} --version failed: {}", p.display(), crate::llm::clip(&out, 200))),
        None => r.fail("Claude Code CLI", format!("{}; install Claude Code (https://claude.com/claude-code)", claude.err().unwrap_or_default())),
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
        r.fail("hooks", format!("none in {} or {}/.claude; run `chloectx install --user` (every session) or `chloectx install` here", user_settings.display(), project.display()));
    } else if user && proj {
        r.ok("hooks", "the project's own install handles this project; the user-level hooks step aside here");
    }

    // The MCP server: user scope (claude mcp) or the project's .mcp.json.
    let mcp_json = read_json(&project.join(".mcp.json"));
    let server = format!("/mcpServers/{}", install::SERVER);
    let old_server = format!("/mcpServers/{}", install::OLD_SERVER);
    if mcp_json.pointer(&server).is_some() {
        r.ok("MCP server", format!("{}/.mcp.json", project.display()));
    } else if proj && mcp_json.pointer(&old_server).is_some() {
        r.warn("MCP server", format!("{}/.mcp.json names it `{}`, its name before the rename; run `chloectx install` again", project.display(), install::OLD_SERVER));
    } else {
        // User-scope servers live in ~/.claude.json (`claude mcp get` would
        // start the server, and with it a store for this directory).
        let cfg_file = paths::claude_json();
        let cfg = read_json(&cfg_file);
        match cfg.pointer(&server) {
            None if user && cfg.pointer(&old_server).is_some() => r.warn(
                "MCP server",
                format!("user scope in {} names it `{}`, its name before the rename; run `chloectx install --user` again", cfg_file.display(), install::OLD_SERVER),
            ),
            Some(s) => {
                let cmd = s.get("command").and_then(|c| c.as_str()).unwrap_or("");
                if Path::new(cmd).is_file() {
                    r.ok("MCP server", format!("user scope in {} ({cmd} mcp)", cfg_file.display()))
                } else {
                    r.fail("MCP server", format!("user scope in {} calls {cmd}, which does not exist; run `chloectx install --user` again", cfg_file.display()))
                }
            }
            None if user || proj => r.fail("MCP server", "not registered; run `chloectx install --user` again, or `claude mcp add-json chloectx '{\"type\":\"stdio\",\"command\":\"<chloectx path>\",\"args\":[\"mcp\"]}' --scope user`"),
            None => r.warn("MCP server", "not registered (ChloeCtx is not installed)"),
        }
    }

    // The embedding model the store will use.
    let store = paths::store_dir(project);
    let cfg = read_json(&store.join("config.json"));
    let spec = cfg.get("embedder").and_then(|v| v.as_str()).map(str::to_string).or_else(|| std::env::var("CHLOECTX_EMBEDDER").ok()).unwrap_or_else(|| "static".into());
    let model_dir = embed::models_dir().join("potion-retrieval-32M");
    if spec == "static" && !model_dir.join("model.safetensors").is_file() {
        r.fail("embedding model", format!("{} is missing; run `chloectx fetch-model`", model_dir.display()));
    } else if spec == "static" {
        r.ok("embedding model", model_dir.display().to_string());
    } else {
        r.ok("embedding model", format!("embedder {spec}"));
    }

    // Distillation: which model, from where.
    let settings_env = |p: &Path| crate::env::in_settings(&read_json(p), "CHLOECTX_DISTILL_MODEL").map(str::to_string);
    let distill = std::env::var("CHLOECTX_DISTILL_MODEL").ok().filter(|m| !m.is_empty()).map(|m| (m, "environment".to_string()))
        .or_else(|| settings_env(&project.join(".claude/settings.local.json")).map(|m| (m, "project settings".into())))
        .or_else(|| settings_env(&user_settings).map(|m| (m, "user settings".into())))
        .or_else(|| cfg.pointer("/distill/model").and_then(|v| v.as_str()).filter(|m| !m.is_empty()).map(|m| (m.to_string(), "store config.json".into())));
    if cfg.pointer("/distill/enabled") == Some(&json!(false)) || std::env::var("CHLOECTX_DISTILL").is_ok_and(|v| v == "0") {
        r.warn("distillation", "off: no decisions, rules or planned work are written down");
    } else {
        match &distill {
            Some((m, from)) => r.ok("distillation", format!("model {m} (from the {from})")),
            None => r.warn("distillation", "uses the Claude Code CLI's default model; a small one is enough (`chloectx install --user --distill-model haiku`)"),
        }
        // The distiller runs `claude -p`, which needs the CLI's own login
        // (an interactive session can work while the CLI's has expired).
        let claude_cmd = crate::llm::resolve_cli("claude").map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "claude".into());
        if let Some((_, out)) = command_output(&claude_cmd, &["auth", "status"]) {
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
                // A failure that is not the login: one small call now says
                // whether it still fails, and with what (CTX-2).
                Some(err) if !crate::health::is_login_error(err) => {
                    r.warn("last distillation", format!("failed {when}: {err}"));
                    let dc: crate::distill::DistillConfig = cfg.get("distill").cloned().and_then(|d| serde_json::from_value(d).ok()).unwrap_or_default();
                    let model = distill.as_ref().map(|(m, _)| m.clone()).unwrap_or_else(|| dc.model());
                    match crate::llm::complete(&dc.cli(&model, 60), "Reply with the single word ok.", "ok") {
                        Ok(_) => r.ok("a model call now", format!("works ({} via {})", if model.is_empty() { "the CLI's default model" } else { &model }, dc.command)),
                        Err(e) => r.fail("a model call now", format!("still fails: {e}")),
                    }
                }
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
            Ok(s) if s.get("error").is_none() => {
                let n = |v: &Json| v.as_u64().unwrap_or(0);
                let plural = |k: u64, w: &str| format!("{k} {w}{}", if k == 1 { "" } else { "s" });
                r.ok(
                    "store",
                    format!("{} ({}, {}; daemon answered in {} ms)", store.display(), plural(n(&s["records"]), "record"), plural(n(&s["sessions"]), "session"), t0.elapsed().as_millis()),
                );
                // The running daemon resolves the CLI with the environment
                // of whatever started it (CTX-3).
                if let Some(e) = s.pointer("/claude/error").and_then(|v| v.as_str()) {
                    if s["distill"] != json!(false) {
                        r.fail("daemon's Claude Code CLI", format!("the running daemon cannot run the model: {e}"));
                    }
                } else if let Some(p) = s.pointer("/claude/path").and_then(|v| v.as_str()) {
                    r.ok("daemon's Claude Code CLI", p);
                }
            }
            Ok(s) => r.fail("store", format!("{}: {}", store.display(), s["error"])),
            Err(e) => r.fail("store", format!("{}: {e}; see {}", store.display(), store.join("daemon.log").display())),
        }
    } else {
        let scratch = std::env::temp_dir().join(format!("chloectx-doctor-{}", std::process::id()));
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

    // Stores of folders that are gone or were temporary pile up.
    let all = crate::stores::survey(&home);
    let now = crate::now();
    let total: u64 = all.iter().map(|e| e.bytes).sum();
    let spare: Vec<&crate::stores::Entry> = all.iter().filter(|e| crate::stores::reason(e, now, None).is_some()).collect();
    let summary = format!("{} store{}, {} in {}", all.len(), if all.len() == 1 { "" } else { "s" }, crate::stores::human(total), home.display());
    if spare.len() >= 10 {
        r.warn("all stores", format!("{summary}; {} of them ({}) belong to folders that are gone or temporary: `chloectx prune` removes them", spare.len(), crate::stores::human(spare.iter().map(|e| e.bytes).sum())));
    } else if !all.is_empty() {
        r.ok("all stores", format!("{summary} (`chloectx stores` lists them)"));
    }

    if r.failed == 0 {
        println!("ChloeCtx is ready. Start `claude` in a project; `chloectx ui` shows what it stores.");
        0
    } else {
        println!("{} check{} failed.", r.failed, if r.failed == 1 { "" } else { "s" });
        1
    }
}
