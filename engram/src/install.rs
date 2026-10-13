//! `engram install`: wires Engram into a project's Claude Code settings.
//!
//! Hooks go to `.claude/settings.local.json` (personal, not committed) by
//! default, or `.claude/settings.json` with `--shared`. The MCP server goes
//! to `.mcp.json` and is pre-approved in the same settings file.

use std::path::Path;

use serde_json::{json, Value as Json};

use crate::Result;

const EVENTS: [(&str, Option<&str>, u64); 9] = [
    ("SessionStart", None, 15),
    ("UserPromptSubmit", None, 10),
    ("PreToolUse", Some("Edit|MultiEdit|Write"), 5),
    ("PostToolUse", None, 10),
    ("PostToolUseFailure", None, 10),
    ("Stop", None, 10),
    ("PreCompact", None, 15),
    ("PostCompact", None, 15),
    ("SessionEnd", None, 10),
];

fn read_json(path: &Path) -> Json {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}))
}

fn write_json(path: &Path, v: &Json) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

/// The hooks block for `exe` (merged into an existing settings object).
pub fn hooks(exe: &str) -> Json {
    let mut h = serde_json::Map::new();
    for (event, matcher, timeout) in EVENTS {
        let mut entry = json!({"hooks": [{"type": "command", "command": format!("{exe} hook {event}"), "timeout": timeout}]});
        if let Some(m) = matcher {
            entry["matcher"] = json!(m);
        }
        h.insert(event.to_string(), json!([entry]));
    }
    Json::Object(h)
}

pub fn install(project: &Path, exe: &str, shared: bool) -> Result<String> {
    let settings_path = project.join(".claude").join(if shared { "settings.json" } else { "settings.local.json" });
    let mut settings = read_json(&settings_path);
    if !settings.is_object() {
        settings = json!({});
    }
    let mut existing = settings.get("hooks").cloned().unwrap_or_else(|| json!({}));
    for (event, entries) in hooks(exe).as_object().unwrap() {
        let list = existing.get(event).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        // Drop earlier Engram entries, keep everything else.
        let mut kept: Vec<Json> = list.into_iter().filter(|e| !e.to_string().contains(" hook ") || !e.to_string().contains("engram")).collect();
        kept.extend(entries.as_array().unwrap().iter().cloned());
        existing[event] = Json::Array(kept);
    }
    settings["hooks"] = existing;
    let mut enabled: Vec<Json> = settings.get("enabledMcpjsonServers").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !enabled.iter().any(|v| v == "engram") {
        enabled.push(json!("engram"));
    }
    settings["enabledMcpjsonServers"] = Json::Array(enabled);
    // The memory tools read and write only Engram's own store.
    if !settings.get("permissions").map_or(false, |p| p.is_object()) {
        settings["permissions"] = json!({});
    }
    let mut allow: Vec<Json> = settings["permissions"].get("allow").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !allow.iter().any(|v| v == "mcp__engram") {
        allow.push(json!("mcp__engram"));
    }
    settings["permissions"]["allow"] = Json::Array(allow);
    write_json(&settings_path, &settings)?;

    let mcp_path = project.join(".mcp.json");
    let mut mcp = read_json(&mcp_path);
    if !mcp.is_object() {
        mcp = json!({});
    }
    if mcp.get("mcpServers").map_or(true, |v| !v.is_object()) {
        mcp["mcpServers"] = json!({});
    }
    mcp["mcpServers"]["engram"] = json!({"command": exe, "args": ["mcp"]});
    write_json(&mcp_path, &mcp)?;
    Ok(format!(
        "Engram installed for {}:\n  hooks and MCP approval: {}\n  MCP server: {}\n  store: {}",
        project.display(),
        settings_path.display(),
        mcp_path.display(),
        crate::paths::store_dir(project).display()
    ))
}
