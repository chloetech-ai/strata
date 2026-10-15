//! `engram install`: wires Engram into Claude Code settings.
//!
//! For one project, hooks go to `.claude/settings.local.json` (personal, not
//! committed) by default, or `.claude/settings.json` with `--shared`. The MCP
//! server goes to `.mcp.json` and is pre-approved in the same settings file.
//!
//! For every session (`--user`), hooks and the permission go to the user
//! settings (`~/.claude/settings.json`) and the MCP server is registered at
//! user scope with `claude mcp add-json`. Those hooks run as
//! `engram hook --user <Event>` and step aside in a project that has its own
//! install, so nothing is recorded twice. Each project still gets its own
//! store, keyed by its directory.
//!
//! `engram uninstall` removes what `install` wrote and keeps the stores.

use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};

use crate::{err, Result};

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

/// A settings file, or `{}` when it is missing or empty; an error when it is
/// not JSON, so that rewriting it cannot lose what it held.
fn read_settings(path: &Path) -> Result<Json> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| err(format!("{} is not valid JSON ({e}); fix it first", path.display()))),
        _ => Ok(json!({})),
    }
}

fn write_json(path: &Path, v: &Json) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

/// Copies `path` to `path.engram-<time>.bak` before it is rewritten.
fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".engram-{}.bak", crate::now()));
    let to = path.with_file_name(name);
    std::fs::copy(path, &to)?;
    Ok(Some(to))
}

/// Whether a hooks entry calls Engram.
fn is_engram_hook(entry: &Json) -> bool {
    let s = entry.to_string();
    s.contains(" hook ") && s.contains("engram")
}

/// Whether a hooks entry is a user-level Engram hook.
fn is_user_hook(entry: &Json) -> bool {
    is_engram_hook(entry) && entry.to_string().contains(" hook --user ")
}

/// The hooks block for `exe` (merged into an existing settings object).
pub fn hooks(exe: &str, user: bool) -> Json {
    let flag = if user { " --user" } else { "" };
    let mut h = serde_json::Map::new();
    for (event, matcher, timeout) in EVENTS {
        let mut entry = json!({"hooks": [{"type": "command", "command": format!("{exe} hook{flag} {event}"), "timeout": timeout}]});
        if let Some(m) = matcher {
            entry["matcher"] = json!(m);
        }
        h.insert(event.to_string(), json!([entry]));
    }
    Json::Object(h)
}

fn push_unique(v: &mut Json, key: &str, item: &str) {
    let mut list: Vec<Json> = v.get(key).and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !list.iter().any(|v| v == item) {
        list.push(json!(item));
    }
    v[key] = Json::Array(list);
}

fn remove_item(v: &mut Json, key: &str, item: &str) {
    if let Some(list) = v.get_mut(key).and_then(|l| l.as_array_mut()) {
        list.retain(|v| v != item);
        if list.is_empty() {
            v.as_object_mut().unwrap().remove(key);
        }
    }
}

/// Adds Engram's hooks, the memory tools' permission and (for a project) the
/// MCP approval to a settings object, replacing earlier Engram entries.
pub fn add_to_settings(settings: &mut Json, exe: &str, user: bool, distill_model: Option<&str>) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let mut existing = settings.get("hooks").cloned().filter(|h| h.is_object()).unwrap_or_else(|| json!({}));
    for (event, entries) in hooks(exe, user).as_object().unwrap() {
        let list = existing.get(event).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        // Drop earlier Engram entries, keep everything else.
        let mut kept: Vec<Json> = list.into_iter().filter(|e| !is_engram_hook(e)).collect();
        kept.extend(entries.as_array().unwrap().iter().cloned());
        existing[event] = Json::Array(kept);
    }
    settings["hooks"] = existing;
    if !user {
        push_unique(settings, "enabledMcpjsonServers", "engram");
    }
    // The memory tools read and write only Engram's own store.
    if !settings.get("permissions").map_or(false, |p| p.is_object()) {
        settings["permissions"] = json!({});
    }
    push_unique(&mut settings["permissions"], "allow", "mcp__engram");
    if let Some(m) = distill_model.filter(|m| !m.is_empty()) {
        if !settings.get("env").map_or(false, |e| e.is_object()) {
            settings["env"] = json!({});
        }
        settings["env"]["ENGRAM_DISTILL_MODEL"] = json!(m);
    }
}

/// Removes everything [`add_to_settings`] adds. Returns whether anything changed.
pub fn remove_from_settings(settings: &mut Json) -> bool {
    let before = settings.clone();
    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for list in hooks.values_mut() {
            if let Some(l) = list.as_array_mut() {
                l.retain(|e| !is_engram_hook(e));
            }
        }
        hooks.retain(|_, l| l.as_array().map_or(true, |l| !l.is_empty()));
        if hooks.is_empty() {
            settings.as_object_mut().unwrap().remove("hooks");
        }
    }
    if settings.is_object() {
        remove_item(settings, "enabledMcpjsonServers", "engram");
    }
    if let Some(p) = settings.get_mut("permissions").filter(|p| p.is_object()) {
        remove_item(p, "allow", "mcp__engram");
    }
    if let Some(env) = settings.get_mut("env").and_then(|e| e.as_object_mut()) {
        env.remove("ENGRAM_DISTILL_MODEL");
    }
    if let Some(obj) = settings.as_object_mut() {
        obj.retain(|k, v| !((k == "permissions" || k == "env") && v.as_object().map_or(false, |o| o.is_empty())));
    }
    *settings != before
}

/// Whether the project's own settings install Engram's hooks. The user
/// settings are skipped even when they are the project's (a session in the
/// home directory).
pub fn project_has_hooks(project: &Path) -> bool {
    let user = crate::paths::user_settings().canonicalize().ok();
    ["settings.json", "settings.local.json"].iter().any(|f| {
        let path = project.join(".claude").join(f);
        if path.canonicalize().ok().map_or(true, |p| Some(p) == user) {
            return false;
        }
        let settings = read_json(&path);
        let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else { return false };
        hooks.values().filter_map(|l| l.as_array()).flatten().any(|e| is_engram_hook(e) && !is_user_hook(e))
    })
}

pub fn install(project: &Path, exe: &str, shared: bool, distill_model: Option<&str>) -> Result<String> {
    let settings_path = project.join(".claude").join(if shared { "settings.json" } else { "settings.local.json" });
    let mut settings = read_settings(&settings_path)?;
    add_to_settings(&mut settings, exe, false, distill_model);
    write_json(&settings_path, &settings)?;

    let mcp_path = project.join(".mcp.json");
    let mut mcp = read_settings(&mcp_path)?;
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

/// Runs `claude mcp <args>`; `Err` carries what went wrong.
fn claude_mcp(args: &[&str]) -> std::result::Result<(), String> {
    let out = std::process::Command::new("claude").arg("mcp").args(args).output().map_err(|e| format!("claude: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Installs Engram for every Claude Code session of this user.
pub fn install_user(exe: &str, distill_model: Option<&str>) -> Result<String> {
    let settings_path = crate::paths::user_settings();
    let mut settings = read_settings(&settings_path)?;
    add_to_settings(&mut settings, exe, true, distill_model);

    // Before the hooks are written: `claude mcp` runs them too.
    let server = json!({"type": "stdio", "command": exe, "args": ["mcp"]}).to_string();
    let _ = claude_mcp(&["remove", "engram", "--scope", "user"]);
    let mcp = match claude_mcp(&["add-json", "engram", &server, "--scope", "user"]) {
        Ok(()) => "registered at user scope (claude mcp get engram)".to_string(),
        Err(e) => format!("not registered ({e}); run: claude mcp add-json engram '{server}' --scope user"),
    };
    let saved = backup(&settings_path)?;
    write_json(&settings_path, &settings)?;
    Ok(format!(
        "Engram installed for every Claude Code session:\n  hooks and permission: {}{}\n  MCP server: {mcp}\n  stores: {}/<project>/memory.strata, one per project directory\n  \
         projects with their own `engram install` keep using it",
        settings_path.display(),
        saved.map(|p| format!(" (previous version saved as {})", p.display())).unwrap_or_default(),
        crate::paths::engram_home().join("projects").display(),
    ))
}

/// Removes Engram from a project's settings and `.mcp.json`; keeps the store.
pub fn uninstall(project: &Path) -> Result<String> {
    let mut changed = Vec::new();
    for f in ["settings.local.json", "settings.json"] {
        let path = project.join(".claude").join(f);
        let mut settings = read_settings(&path)?;
        if path.exists() && remove_from_settings(&mut settings) {
            write_json(&path, &settings)?;
            changed.push(path);
        }
    }
    let mcp_path = project.join(".mcp.json");
    let mut mcp = read_json(&mcp_path);
    if let Some(servers) = mcp.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
        if servers.remove("engram").is_some() {
            write_json(&mcp_path, &mcp)?;
            changed.push(mcp_path);
        }
    }
    if changed.is_empty() {
        return Ok(format!("Engram was not installed in {}", project.display()));
    }
    Ok(format!(
        "Engram removed from {}\n  store kept: {}",
        changed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
        crate::paths::store_dir(project).display()
    ))
}

/// Removes the user-level install; keeps the stores.
pub fn uninstall_user() -> Result<String> {
    let settings_path = crate::paths::user_settings();
    let mut settings = read_settings(&settings_path)?;
    let hooks = if settings_path.exists() && remove_from_settings(&mut settings) {
        let saved = backup(&settings_path)?;
        write_json(&settings_path, &settings)?;
        format!("removed from {}{}", settings_path.display(), saved.map(|p| format!(" (previous version saved as {})", p.display())).unwrap_or_default())
    } else {
        format!("none in {}", settings_path.display())
    };
    let mcp = match claude_mcp(&["remove", "engram", "--scope", "user"]) {
        Ok(()) => "removed".to_string(),
        Err(e) => format!("not removed ({e})"),
    };
    Ok(format!(
        "Engram uninstalled for every session:\n  hooks: {hooks}\n  MCP server: {mcp}\n  stores kept in {}",
        crate::paths::engram_home().join("projects").display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(settings: &Json) -> Vec<String> {
        settings["hooks"].as_object().unwrap().values().flat_map(|l| l.as_array().unwrap()).map(|e| e["hooks"][0]["command"].as_str().unwrap_or("").to_string()).collect()
    }

    #[test]
    fn user_install_merges_and_uninstall_restores() {
        let original = json!({
            "model": "sonnet",
            "env": {"DISABLE_AUTOUPDATER": "1"},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]},
            "permissions": {"allow": ["Bash(ls:*)"]}
        });
        let mut s = original.clone();
        add_to_settings(&mut s, "/bin/engram", true, Some("haiku"));
        add_to_settings(&mut s, "/bin/engram", true, Some("haiku")); // idempotent
        let cmds = commands(&s);
        assert_eq!(cmds.iter().filter(|c| c.contains("engram hook --user ")).count(), EVENTS.len());
        assert!(cmds.contains(&"say done".to_string()));
        assert_eq!(s["permissions"]["allow"], json!(["Bash(ls:*)", "mcp__engram"]));
        assert_eq!(s["env"]["ENGRAM_DISTILL_MODEL"], "haiku");
        assert!(s.get("enabledMcpjsonServers").is_none());
        assert!(remove_from_settings(&mut s));
        assert_eq!(s, original);
        assert!(!remove_from_settings(&mut s));
    }

    #[test]
    fn project_uninstall_removes_only_engram() {
        let mut s = json!({"enabledMcpjsonServers": ["other"]});
        add_to_settings(&mut s, "/x/engram", false, None);
        assert_eq!(s["enabledMcpjsonServers"], json!(["other", "engram"]));
        assert!(commands(&s).iter().all(|c| c.starts_with("/x/engram hook ") && !c.contains("--user")));
        remove_from_settings(&mut s);
        assert_eq!(s, json!({"enabledMcpjsonServers": ["other"]}));
    }

    #[test]
    fn user_hooks_step_aside_only_for_a_project_install() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        assert!(!project_has_hooks(project));
        let local = project.join(".claude/settings.local.json");
        let mut s = json!({});
        add_to_settings(&mut s, "/bin/engram", true, None);
        write_json(&local, &s).unwrap();
        assert!(!project_has_hooks(project), "user-level hooks copied into a project are not a project install");
        install(project, "/bin/engram", false, None).unwrap();
        assert!(project_has_hooks(project));
        uninstall(project).unwrap();
        assert!(!project_has_hooks(project));
        assert_eq!(read_json(&project.join(".mcp.json")), json!({"mcpServers": {}}));
    }
}
