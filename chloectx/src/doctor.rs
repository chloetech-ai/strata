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
    } else if events.len() < 9 {
        r.warn(what, format!("{} has ChloeCtx hooks for {} events only ({}); run `chloectx install` again", path.display(), events.len(), events.join(", ")));
    } else {
        r.ok(what, format!("{} ({} events, {exe})", path.display(), events.len()));
    }
    true
}

/// Checks the plugin that the settings enable: installed, with its
/// launcher, and set up. Returns whether its hooks will run.
fn check_plugin(r: &mut Report, p: &crate::plugin::Found) -> bool {
    let id = &p.id;
    let Some(launcher) = p.launcher() else {
        r.fail("plugin", format!("{} enables {id}, but Claude Code has not installed it on this machine; run `claude plugin install {id}`", p.settings.display()));
        return false;
    };
    let executable = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&launcher).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            launcher.is_file()
        }
    };
    if !executable {
        r.fail("plugin", format!("{id} is enabled, but its launcher {} is missing or not executable; run `claude plugin update {id}` (or uninstall and install it again)", launcher.display()));
        return false;
    }
    let events = p.hook_events();
    let version = p.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
    r.ok("plugin", format!("{id}{version}, enabled in {} ({} hook events, through {})", p.settings.display(), events.len(), launcher.display()));
    // What the launcher's setup left: the program it downloaded and the
    // model, or why it could not.
    let data = p.data_dir(&crate::plugin::plugins_root());
    if let Ok(why) = std::fs::read_to_string(data.join("failed")) {
        r.fail("plugin setup", format!("{}; see {}, and `{} setup` tries again in this shell", why.trim(), data.join("setup.log").display(), launcher.display()));
    } else if data.join("setup.lock").is_dir() && !data.join("ready").is_file() {
        r.warn("plugin setup", format!("running: it downloads chloectx and the embedding model once (see {})", data.join("setup.log").display()));
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

/// How far back `doctor` counts the model calls a store logged.
const MEASURED_DAYS: i64 = 30;

/// The events of the store's `events.log` (its last 4 MB) of the last
/// `days` days.
fn recent_events(store: &Path, days: i64) -> Vec<Json> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(store.join("events.log")) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(4 << 20))).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let since = crate::now() - days * 86_400;
    buf.split(|&b| b == b'\n').filter_map(|l| serde_json::from_slice::<Json>(l).ok()).filter(|e| e["ts"].as_i64().is_some_and(|t| t >= since)).collect()
}

/// What the logged `event`s took and cost: how many, the median time,
/// the dollars in all and each, and how many failed.
fn measured(events: &[Json], event: &str, one: &str, many: &str) -> String {
    let es: Vec<&Json> = events.iter().filter(|e| e["event"] == event).collect();
    if es.is_empty() {
        return format!("nothing measured in this store's last {MEASURED_DAYS} days yet (about $0.001 a {one} with haiku)");
    }
    let failed = es.iter().filter(|e| e.get("error").is_some_and(|v| !v.is_null())).count();
    let done: Vec<&&Json> = es.iter().filter(|e| e.get("error").map_or(true, |v| v.is_null())).collect();
    let cost: f64 = done.iter().filter_map(|e| e["cost_usd"].as_f64()).sum();
    let mut ms: Vec<u64> = done.iter().filter_map(|e| e["ms"].as_u64()).collect();
    ms.sort_unstable();
    let n = es.len();
    let mut s = format!("{n} {} in the last {MEASURED_DAYS} days", if n == 1 { one } else { many });
    if let Some(m) = ms.get(ms.len() / 2).filter(|_| !ms.is_empty()) {
        s += &format!(", median {:.1} s", *m as f64 / 1000.0);
    }
    if !done.is_empty() {
        s += &format!(", ${cost:.3} in all (${:.4} a {one})", cost / done.len() as f64);
    }
    if failed > 0 {
        s += &format!("; {failed} failed");
    }
    s
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

    // Where the hooks come from: the plugin, the user settings, the
    // project, or more than one of them.
    let user_settings = paths::user_settings();
    let plugin = crate::plugin::enabled(project);
    let plugin_ok = plugin.as_ref().is_some_and(|p| check_plugin(&mut r, p));
    let user = check_hooks(&mut r, "user hooks", &user_settings);
    let mut proj = false;
    for f in ["settings.local.json", "settings.json"] {
        let p = project.join(".claude").join(f);
        if p.canonicalize().ok() != user_settings.canonicalize().ok() {
            proj |= check_hooks(&mut r, "project hooks", &p);
        }
    }
    if !user && !proj && plugin.is_none() {
        r.fail(
            "hooks",
            format!(
                "none in {} or {}/.claude, and the {} plugin is not enabled; install the plugin (`/plugin install {}@{} --marketplace chloetech-ai/chloedb` in Claude Code), or run `chloectx install --user` (every session) or `chloectx install` here",
                user_settings.display(),
                project.display(),
                crate::plugin::NAME,
                crate::plugin::NAME,
                crate::plugin::MARKETPLACE
            ),
        );
    } else if proj && (user || plugin_ok) {
        r.ok("hooks", format!("the project's own install handles this project; the {} hooks step aside here", if user { "user-level" } else { "plugin's" }));
    } else if user && plugin_ok {
        r.warn("hooks", "both the plugin and `chloectx install --user` are installed: the user settings' hooks record and the plugin's step aside; `chloectx uninstall --user` leaves the plugin alone");
    }

    // The MCP server: user scope (claude mcp) or the project's .mcp.json.
    let mcp_json = read_json(&project.join(".mcp.json"));
    let server = format!("/mcpServers/{}", install::SERVER);
    let old_server = format!("/mcpServers/{}", install::OLD_SERVER);
    let user_server = read_json(&paths::claude_json()).pointer(&server).is_some();
    if plugin_ok && mcp_json.pointer(&server).is_none() && !user_server {
        r.ok("MCP server", format!("the plugin's, plugin:{}:{} (its tools are {}*)", crate::plugin::NAME, install::SERVER, crate::plugin::TOOL_PREFIX));
    } else if mcp_json.pointer(&server).is_some() {
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

    if plugin_ok && (user_server || mcp_json.pointer(&server).is_some()) {
        r.warn("MCP server", format!("the plugin serves the memory tools too, so a session has them twice (`mcp__{}__*` and `{}*`); `chloectx uninstall --user` (or `chloectx uninstall` in the project) leaves only the plugin's", install::SERVER, crate::plugin::TOOL_PREFIX));
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
        .or_else(|| cfg.pointer("/distill/model").and_then(|v| v.as_str()).filter(|m| !m.is_empty()).map(|m| (m.to_string(), "store config.json".into())))
        // The plugin's launcher sets haiku when nothing else names a model.
        .or_else(|| plugin_ok.then(|| ("haiku".to_string(), "plugin's default".into())));
    // A setting as the hooks see it: their environment is Claude Code's,
    // which adds the settings files' `env` to the one it was started with.
    let setting = |k: &str| -> Option<(String, String)> {
        std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (v, "environment".to_string())).or_else(|| {
            [(project.join(".claude/settings.local.json"), "project's local settings"), (project.join(".claude/settings.json"), "project settings"), (user_settings.clone(), "user settings")]
                .iter()
                .find_map(|(p, from)| crate::env::in_settings(&read_json(p), k).filter(|v| !v.is_empty()).map(|v| (v.to_string(), from.to_string())))
        })
    };
    let distill_off = cfg.pointer("/distill/enabled") == Some(&json!(false)) || setting("CHLOECTX_DISTILL").is_some_and(|(v, _)| v == "0");
    let deep_cfg: crate::deep::DeepConfig = cfg.get("deep").cloned().and_then(|d| serde_json::from_value(d).ok()).unwrap_or_default();
    let deep = deep_cfg.resolved_with(|k| setting(k).map(|(v, _)| v));
    let deep_calls = usize::from(deep.planner == "model") + usize::from(deep.ranker == "model");
    let deep_calls_on = deep.automatic() && deep_calls > 0;
    if distill_off {
        r.warn("distillation", "off: no decisions, rules or planned work are written down");
    } else {
        match &distill {
            Some((m, from)) => r.ok("distillation", format!("model {m} (from the {from}), after each turn")),
            None => r.warn("distillation", "uses the Claude Code CLI's default model; a small one is enough (`chloectx install --user --distill-model haiku`)"),
        }
    }
    // The distiller and query understanding run `claude -p`, which needs
    // the CLI's own login (an interactive session can work while the
    // CLI's has expired).
    if !distill_off || deep_calls_on {
        let who = match (!distill_off, deep_calls_on) {
            (true, true) => "the distiller and query understanding",
            (true, false) => "the distiller",
            _ => "query understanding",
        };
        match crate::llm::resolve_cli("claude") {
            Err(e) => r.fail("Claude Code login", format!("{who} cannot run: {e}")),
            Ok(cli) => match command_output(&cli.to_string_lossy(), &["auth", "status"]) {
                None => r.fail("Claude Code login", format!("{who} cannot run: {} does not start", cli.display())),
                Some((_, out)) => match serde_json::from_str::<Json>(&out).ok().and_then(|st| st.get("loggedIn").and_then(|v| v.as_bool())) {
                    Some(false) => r.fail("Claude Code login", format!("the `claude` CLI is logged out, so {who} cannot run (every model call fails); run `claude auth login`")),
                    Some(true) => r.ok("Claude Code login", format!("logged in, so {who} can run")),
                    None => r.warn("Claude Code login", format!("`{} auth status` did not say whether it is logged in ({}); {who} needs it to be", cli.display(), crate::llm::clip(&out, 160))),
                },
            },
        }
    }

    // Query understanding: when it runs, what waits for it, and what it
    // cost here (from the store's events.log).
    let mode_from = setting("CHLOECTX_DEEP")
        .map(|(_, f)| format!("CHLOECTX_DEEP in the {f}"))
        .or_else(|| cfg.pointer("/deep/mode").map(|_| "the store's config.json".to_string()))
        .unwrap_or_else(|| "the default".into());
    let models = match (deep.planner == "model", deep.ranker == "model") {
        (true, true) if deep.planner_model == deep.ranker_model => format!("2 model calls ({}: one plans the searches, one picks the records)", deep.planner_model),
        (true, true) => format!("2 model calls (planner {}, ranker {})", deep.planner_model, deep.ranker_model),
        (true, false) => format!("1 model call (planner {}; the records are ranked here)", deep.planner_model),
        (false, true) => format!("1 model call (ranker {}; no planner)", deep.ranker_model),
        (false, false) => "no model calls (planner none, ranker local)".to_string(),
    };
    let when = format!("after each prompt of {} words or more, once the store holds {} records", deep.min_words, deep.min_records);
    match deep.mode.as_str() {
        "off" => r.ok("query understanding", format!("off ({mode_from}): no model calls after prompts")),
        "tool" => r.ok("query understanding", format!("only when the model asks for it (memory_ask with deep; {mode_from}): no model calls after prompts")),
        "sync" => r.warn(
            "query understanding",
            format!("at prompt time ({mode_from}): {when}, the prompt waits for {models}, up to {} s each, before Claude starts; CHLOECTX_DEEP=async runs it in the background", deep.timeout_secs),
        ),
        "async" if deep.stop_recheck => r.warn(
            "query understanding",
            format!("after the prompt, in the background ({mode_from}): {when}, {models}; at the end of a turn it waits up to {} s for a search still running and may ask Claude to check its answer, which runs the main model again (stop_recheck is on)", deep.stop_wait().as_secs()),
        ),
        "async" => r.ok(
            "query understanding",
            format!("after the prompt, in the background ({mode_from}): {when}, {models}; nothing waits for it: what it finds comes with the turn's next tool result or the next prompt, and the end of a turn does not wait (stop_recheck off)"),
        ),
        m => r.warn("query understanding", format!("mode {m:?} ({mode_from}) is none of async, sync, tool or off: the hooks treat it as off")),
    }
    let events = recent_events(&store, MEASURED_DAYS);
    if deep_calls_on || events.iter().any(|e| e["event"] == "deep") {
        r.ok("query understanding cost", measured(&events, "deep", "search", "searches"));
    }
    if !distill_off || events.iter().any(|e| e["event"] == "distill") {
        r.ok("distillation cost", measured(&events, "distill", "turn", "turns"));
    }
    if !distill_off {
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
