//! ChloeCtx in every session (SPEC.md, "ChloeCtx is required"). Most of
//! what Workbench shows above the terminal comes from ChloeCtx: decisions,
//! blame, recall, the story's "what it knew". So each session Workbench
//! starts gets ChloeCtx's hooks and MCP server in its own `--settings` and
//! `--mcp-config` files, from `chloectx wiring`, unless the user's or the
//! project's settings already wire it (then nothing is added, and the
//! session is recorded once). Nothing of the user's is written.
//!
//! Without a usable ChloeCtx, `serve` still starts and the UI shows a setup
//! screen; starting an agent is refused with `chloectx_missing` and the fix.
//! `--no-chloectx` (hidden, for tests) turns this off.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};

use crate::server::{blocking, ApiResult, App};
use crate::supervisor::ApiError;
use crate::trail::{run_chloectx, run_raw, Ran};

/// The command that installs both programs, from a checkout.
pub const FIX: &str = "./install.sh --workbench   (from a checkout of https://github.com/chloetech-ai/chloedb)";

/// The ChloeCtx program to run: `bin` as given when it is a path; else
/// found on PATH; else `~/.local/bin/chloectx`, where install.sh puts it
/// (a shell that started Workbench may not have that on PATH).
pub fn resolve(bin: &str) -> String {
    if bin.contains('/') {
        return bin.to_string();
    }
    let on_path = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(bin)).find(|f| is_exec(f)));
    if let Some(p) = on_path {
        return p.to_string_lossy().into_owned();
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".local/bin").join(bin);
        if is_exec(&p) {
            return p.to_string_lossy().into_owned();
        }
    }
    bin.to_string()
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// What a session in `cwd` gets from ChloeCtx (`chloectx wiring`), or why
/// it cannot get it, in a sentence with the fix.
pub fn wiring(bin: &str, cwd: &Path) -> Result<Json, String> {
    let exe = if bin.contains('/') { std::fs::canonicalize(bin).map(|p| p.to_string_lossy().into_owned()).unwrap_or(bin.to_string()) } else { bin.to_string() };
    let cwd_s = cwd.to_string_lossy().into_owned();
    match run_chloectx(bin, cwd, &["wiring", "--project", &cwd_s, "--exe", &exe], Duration::from_secs(20)) {
        Ran::Out(w) if w.get("settings").is_some() => Ok(w),
        Ran::Out(_) | Ran::Failed(_) => Err(format!("{bin} is too old for Workbench: it has no `wiring` command")),
        Ran::Missing => Err(format!("ChloeCtx is not installed: {bin} was not found")),
    }
}

/// The check the setup screen shows: whether sessions will get ChloeCtx.
pub fn status(bin: &str, required: bool, cwd: &Path) -> Json {
    if !required {
        return json!({"required": false, "ok": true, "bin": bin});
    }
    let version = match run_raw(bin, cwd, &["--version"], Duration::from_secs(10)) {
        Ran::Out(Json::String(v)) => v.trim().to_string(),
        _ => String::new(),
    };
    match wiring(bin, cwd) {
        Ok(w) => {
            let model_ok = w.pointer("/model/ok").and_then(Json::as_bool).unwrap_or(true);
            let problem = (!model_ok).then(|| format!("ChloeCtx's embedding model is missing ({}), so recall finds nothing", w.pointer("/model/path").and_then(Json::as_str).unwrap_or("")));
            json!({
                "required": true,
                "ok": model_ok,
                "bin": bin,
                "version": version,
                "wired": w["wired"],
                "model": w["model"],
                "problem": problem,
                "fix": (!model_ok).then(|| format!("{bin} fetch-model")),
            })
        }
        Err(why) => json!({"required": true, "ok": false, "bin": bin, "version": version, "problem": why, "fix": "./install.sh --workbench"}),
    }
}

/// `base` (a session's settings) with ChloeCtx's `add` merged in: hooks
/// appended per event, permissions' allow joined, env added.
pub fn merge_settings(mut base: Json, add: &Json) -> Json {
    if let Some(hooks) = add.get("hooks").and_then(Json::as_object) {
        if !base["hooks"].is_object() {
            base["hooks"] = json!({});
        }
        for (event, entries) in hooks {
            let list = base["hooks"].get(event).and_then(Json::as_array).cloned().unwrap_or_default();
            let mut list = list;
            list.extend(entries.as_array().cloned().unwrap_or_default());
            base["hooks"][event] = Json::Array(list);
        }
    }
    if let Some(allow) = add.pointer("/permissions/allow").and_then(Json::as_array) {
        if !base["permissions"].is_object() {
            base["permissions"] = json!({});
        }
        let mut list = base["permissions"].get("allow").and_then(Json::as_array).cloned().unwrap_or_default();
        for a in allow {
            if !list.contains(a) {
                list.push(a.clone());
            }
        }
        base["permissions"]["allow"] = Json::Array(list);
    }
    if let Some(env) = add.get("env").and_then(Json::as_object) {
        if !base["env"].is_object() {
            base["env"] = json!({});
        }
        for (k, v) in env {
            base["env"][k] = v.clone();
        }
    }
    base
}

/// `base` (a session's MCP config) with ChloeCtx's servers added.
pub fn merge_mcp(mut base: Json, add: &Json) -> Json {
    if let Some(servers) = add.as_object() {
        for (name, s) in servers {
            base["mcpServers"][name] = s.clone();
        }
    }
    base
}

/// The settings and MCP config of a new session in `cwd`, wired to ChloeCtx
/// when it is required.
pub fn wire(bin: &str, required: bool, cwd: &Path, settings: Json, mcp: Json) -> Result<(Json, Json), ApiError> {
    if !required {
        return Ok((settings, mcp));
    }
    let w = wiring(bin, cwd).map_err(missing)?;
    Ok((merge_settings(settings, &w["settings"]), merge_mcp(mcp, &w["mcp"])))
}

/// [`wire`], also telling ChloeCtx who the session is ([`Who`]): its
/// agent name, who wrote its task, and where later prompts' sources are
/// noted. The task is noted before the session starts.
// Called from spawn and Supervisor::input once those take it (SPEC.md,
// "Sixth build: ChloeCtx remembers the right things", integration).
pub fn wire_as(bin: &str, required: bool, cwd: &Path, settings: Json, mcp: Json, who: &Who) -> Result<(Json, Json), ApiError> {
    let settings = merge_settings(settings, &json!({ "env": who.env() }));
    let out = wire(bin, required, cwd, settings, mcp)?;
    who.started();
    Ok(out)
}

/// What Workbench tells ChloeCtx about a session, so that it records who
/// said what (SPEC.md, "Sixth build: ChloeCtx remembers the right
/// things"): the agent's name (`CHLOECTX_AGENT_NAME`), who wrote the task
/// it starts with (`CHLOECTX_PROMPT_SOURCE`), and a file where Workbench
/// notes, before typing it, who wrote each later prompt that is not the
/// user's (`CHLOECTX_PROMPT_LOG`, see [`note_prompt`]). ChloeCtx never takes
/// an agent's or Workbench's words for the user's preferences.
// Called from spawn and Supervisor::input once those take it (SPEC.md,
// "Sixth build: ChloeCtx remembers the right things", integration).
#[derive(Clone, Debug)]
pub struct Who {
    pub agent: String,
    /// `user`, `workbench` or `agent:<name>`.
    pub source: String,
    pub log: PathBuf,
    pub task: Option<String>,
}

// Called from spawn and Supervisor::input once those take it (SPEC.md,
// "Sixth build: ChloeCtx remembers the right things", integration).
impl Who {
    /// The session `session_dir` of the agent `name`: its task written by
    /// `parent` (another agent's name) when one started it; by Workbench
    /// for a goal's lead (its brief is Workbench's text); else by the user.
    pub fn new(name: &str, session_dir: &Path, parent: Option<&str>, lead_of_goal: bool, task: Option<&str>) -> Who {
        let source = match parent {
            Some(p) => format!("agent:{p}"),
            None if lead_of_goal => "workbench".into(),
            None => "user".into(),
        };
        Who { agent: name.to_string(), source, log: prompt_log(session_dir), task: task.map(String::from) }
    }

    pub fn env(&self) -> Json {
        json!({
            "CHLOECTX_AGENT_NAME": self.agent,
            "CHLOECTX_PROMPT_SOURCE": self.source,
            "CHLOECTX_PROMPT_LOG": self.log.to_string_lossy(),
        })
    }

    /// Notes the task the session starts with, when it is not the user's.
    fn started(&self) {
        if let Some(t) = self.task.as_deref().filter(|_| self.source != "user") {
            note_prompt(&self.log, t, &self.source);
        }
    }
}

/// The file of a session's prompt sources.
// Called from spawn and Supervisor::input once those take it (SPEC.md,
// "Sixth build: ChloeCtx remembers the right things", integration).
pub fn prompt_log(session_dir: &Path) -> PathBuf {
    session_dir.join("prompt-sources.jsonl")
}

/// Notes, before Workbench types `text` into a session, who wrote it
/// (`agent:<name>` for `send_to_agent`, `workbench` for its own briefs):
/// ChloeCtx's hook matches the prompt's start against the newest lines.
/// The user's own prompts need no line. Best effort: a prompt it could
/// not note is taken for the user's, as it was before.
// Called from spawn and Supervisor::input once those take it (SPEC.md,
// "Sixth build: ChloeCtx remembers the right things", integration).
pub fn note_prompt(log: &Path, text: &str, source: &str) {
    use std::io::Write;
    let start: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
    if start.is_empty() {
        return;
    }
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let line = json!({ "ts": ts, "text": start, "source": source });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(f, "{line}");
    }
}

/// The answer to a spawn without ChloeCtx: why, and the fix.
pub fn missing(why: String) -> ApiError {
    ApiError::new(409, "chloectx_missing", format!("{why}. Workbench needs it; run {FIX}"))
}

pub fn routes() -> Router<App> {
    Router::new().route("/api/chloectx", get(status_route))
}

async fn status_route(State(app): State<App>) -> ApiResult {
    let sup = app.sup.clone();
    let st = blocking(move || Ok(status(&sup.cfg.chloectx_bin, sup.cfg.chloectx_required, &sup.cfg.data_dir))).await?;
    Ok(AxJson(st).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_chloectx_into_a_sessions_files() {
        let base = crate::supervisor::settings_json("/wb");
        let add = json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/c hook Stop", "timeout": 30}]}], "PostCompact": [{"hooks": []}]},
            "permissions": {"allow": ["mcp__chloectx"]},
            "env": {"CHLOECTX_DISTILL_MODEL": "haiku"},
        });
        let s = merge_settings(base, &add);
        let stop = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "{s}");
        assert!(stop[0]["hooks"][0]["command"].as_str().unwrap().contains("/wb"));
        assert_eq!(stop[1]["hooks"][0]["command"], "/c hook Stop");
        assert!(s["hooks"]["PostCompact"].is_array());
        assert_eq!(s["permissions"]["allow"], json!(["mcp__chloectx"]));
        assert_eq!(s["env"]["CHLOECTX_DISTILL_MODEL"], "haiku");
        let m = merge_mcp(crate::supervisor::mcp_json("/wb", "http://x", "t", "a1"), &json!({"chloectx": {"command": "/c", "args": ["mcp"]}}));
        assert_eq!(m["mcpServers"]["workbench"]["command"], "/wb");
        assert_eq!(m["mcpServers"]["chloectx"]["command"], "/c");
    }

    #[test]
    fn sessions_say_who_wrote_their_prompts() {
        let dir = tempfile::tempdir().unwrap();
        // An agent the lead started: its task is the lead's, noted before it runs.
        let who = Who::new("freeship", dir.path(), Some("lead"), false, Some("Add free shipping.\nShipping is 499 by default."));
        let (s, _) = wire_as("chloectx", false, dir.path(), json!({}), json!({}), &who).unwrap();
        assert_eq!(s["env"]["CHLOECTX_AGENT_NAME"], "freeship");
        assert_eq!(s["env"]["CHLOECTX_PROMPT_SOURCE"], "agent:lead");
        assert_eq!(s["env"]["CHLOECTX_PROMPT_LOG"], prompt_log(dir.path()).to_string_lossy().as_ref());
        let log = std::fs::read_to_string(prompt_log(dir.path())).unwrap();
        let line: Json = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(line["text"], "Add free shipping. Shipping is 499 by default.");
        assert_eq!(line["source"], "agent:lead");
        // A goal's lead gets Workbench's brief; the user's own task is not noted.
        assert_eq!(Who::new("lead", dir.path(), None, true, None).source, "workbench");
        let other = tempfile::tempdir().unwrap();
        let mine = Who::new("cents", other.path(), None, false, Some("Fix the cent bug."));
        assert_eq!(mine.source, "user");
        wire_as("chloectx", false, other.path(), json!({}), json!({}), &mine).unwrap();
        assert!(!prompt_log(other.path()).exists());
        // A later message from another agent.
        note_prompt(&prompt_log(other.path()), "  Stop after   the tests pass. ", "agent:lead");
        assert!(std::fs::read_to_string(prompt_log(other.path())).unwrap().contains("\"text\":\"Stop after the tests pass.\""));
    }
}
