//! `chloectx hook <event>`: reads the hook payload on stdin, forwards it to the
//! project's daemon, and prints what the daemon wants injected. It never
//! fails the hook: on any error it prints nothing and exits 0.
//!
//! `chloectx hook --user <event>` is the same hook from the user settings; it
//! does nothing in a project that installs ChloeCtx itself, whose own hook
//! handles the event.
//!
//! The plugin's hooks are `--user` hooks run by its launcher, which sets
//! `CHLOECTX_PLUGIN=1`: they also step aside where the user settings hold
//! the hooks of `chloectx install --user`, so that an install from before
//! the plugin, left in place, does not record each event twice.

use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::{daemon, install, paths};

/// How long each event may wait for the daemon (the first one may start it).
/// With query understanding on, a prompt may wait for it (`sync`), and,
/// only when `stop_recheck` is turned on, the end of a turn for a search
/// still running (a few seconds at most).
fn budget(event: &str, deep: &crate::deep::DeepConfig) -> Duration {
    Duration::from_millis(match event {
        "SessionStart" => 8000,
        "UserPromptSubmit" if deep.mode == "sync" => 1000 + 2 * 1000 * deep.timeout_secs.min(40),
        "UserPromptSubmit" => 3000,
        "Stop" => 2000 + deep.stop_wait().as_millis() as u64,
        "PostCompact" => 5000,
        "PreToolUse" => 1500,
        // Recorded and answered at once; nothing waits on them.
        "Notification" | "SubagentStop" => 1000,
        _ => 2000,
    })
}

/// The store's query-understanding settings (config file and environment).
fn deep_config(dir: &std::path::Path) -> crate::deep::DeepConfig {
    let raw: Json = std::fs::read_to_string(dir.join("config.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null);
    let cfg: crate::deep::DeepConfig = raw.get("deep").cloned().and_then(|d| serde_json::from_value(d).ok()).unwrap_or_default();
    cfg.resolved()
}

pub fn run(event: &str, user: bool) -> i32 {
    if std::env::var("CHLOECTX_DISABLE").map_or(false, |v| v == "1") {
        return 0;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let payload: Json = serde_json::from_str(&input).unwrap_or(Json::Null);
    let event = payload.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or(event).to_string();
    let project = paths::project_dir(payload.get("cwd").and_then(|v| v.as_str()));
    if user && install::project_has_hooks(&project) {
        return 0;
    }
    if crate::plugin::called_by_plugin() && install::user_has_hooks() {
        return 0;
    }
    let dir = paths::store_dir(&project);
    let deep = deep_config(&dir);
    // The session's settings travel with each request: the daemon may have
    // been started from elsewhere (a shell, the MCP server) without them.
    let distill_model = std::env::var("CHLOECTX_DISTILL_MODEL").unwrap_or_default();
    // Where Claude Code's PATH has the CLI, and whether distillation is
    // switched off in its environment (CTX-3).
    let claude = crate::llm::on_path("claude");
    let distill = std::env::var("CHLOECTX_DISTILL").ok().filter(|v| !v.is_empty()).map(|v| v != "0");
    let mut req = json!({"op": "hook", "event": event, "payload": payload, "deep": deep, "distill_model": distill_model, "receipts": daemon::receipts_env(), "claude": claude, "distill": distill});
    // Who wrote a prompt, when Workbench says (see [`prompt_source`]), and
    // the session's agent name.
    if let Ok(a) = std::env::var("CHLOECTX_AGENT_NAME") {
        req["agent"] = json!(a);
    }
    if event == "UserPromptSubmit" {
        let prompt = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        let log = std::env::var_os("CHLOECTX_PROMPT_LOG").map(std::path::PathBuf::from);
        req["source"] = json!(log.and_then(|l| prompt_source(&l, prompt, crate::now() - crate::clock_offset())));
        req["session_source"] = json!(std::env::var("CHLOECTX_PROMPT_SOURCE").ok());
    }
    let wait = budget(&event, &deep);
    match daemon::request(&dir, &project.to_string_lossy(), &req, wait) {
        Ok(resp) => {
            if let Some(out) = resp.get("output").filter(|o| !o.is_null()) {
                println!("{out}");
            }
            if let Some(e) = resp.get("error") {
                eprintln!("chloectx: {e}");
            }
        }
        Err(e) => eprintln!("chloectx: {e}"),
    }
    0
}

/// The source Workbench noted for a prompt in `log` (`CHLOECTX_PROMPT_LOG`):
/// JSON lines `{"ts": <unix seconds>, "text": <the prompt's start>,
/// "source": "user" | "workbench" | "agent:<name>"}`, written just before
/// it types a prompt into the session (another agent's `send_to_agent`, a
/// goal's brief). The newest line of the last hour whose text starts the
/// prompt (spaces aside) is the one; a prompt with no line is the user's.
pub fn prompt_source(log: &std::path::Path, prompt: &str, now: i64) -> Option<String> {
    let norm = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let p = norm(prompt);
    if p.is_empty() {
        return None;
    }
    let text = std::fs::read_to_string(log).ok()?;
    text.lines().rev().take(200).filter_map(|l| serde_json::from_str::<Json>(l).ok()).find_map(|v| {
        let ts = v.get("ts").and_then(|t| t.as_i64()).unwrap_or(0);
        let start = norm(v.get("text").and_then(|t| t.as_str()).unwrap_or(""));
        let src = v.get("source").and_then(|s| s.as_str())?;
        (now - ts <= 3600 && !start.is_empty() && p.starts_with(&start)).then(|| src.to_string())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn prompts_get_the_source_workbench_noted() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("prompt-sources.jsonl");
        std::fs::write(&log, "{\"ts\": 1000, \"text\": \"Add tests  for shipping\", \"source\": \"agent:lead\"}\nnot json\n{\"ts\": 1000, \"text\": \"You lead the work\", \"source\": \"workbench\"}\n").unwrap();
        assert_eq!(super::prompt_source(&log, "Add tests for shipping: 499 by default.", 1100).as_deref(), Some("agent:lead"));
        assert_eq!(super::prompt_source(&log, "You lead the work on a goal", 1100).as_deref(), Some("workbench"));
        assert_eq!(super::prompt_source(&log, "Don't touch money.py", 1100), None);
        // An hour later the note is stale.
        assert_eq!(super::prompt_source(&log, "Add tests for shipping", 5000), None);
        assert_eq!(super::prompt_source(&dir.path().join("none"), "Add tests", 1100), None);
    }
}
