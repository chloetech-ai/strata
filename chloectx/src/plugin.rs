//! ChloeCtx as a Claude Code plugin: `chloectx/plugin/`, listed in the
//! repository's marketplace (`.claude-plugin/marketplace.json`, the
//! marketplace `chloedb`), installed with `/plugin install chloectx@chloedb`.
//!
//! The plugin brings the same nine hooks as `chloectx install --user` and
//! the MCP server, each through its launcher (`scripts/chloectx-launch`),
//! which finds or downloads the chloectx program and runs it with
//! `CHLOECTX_PLUGIN=1` set. Claude Code names the plugin server's tools
//! `mcp__plugin_chloectx_chloectx__*`.
//!
//! What is here reads what Claude Code wrote about the plugin, for `doctor`:
//! whether a settings file enables it (`enabledPlugins`) and where it is
//! installed (`installed_plugins.json` under its plugins folder).

use std::path::{Path, PathBuf};

use serde_json::Value as Json;

/// The plugin's name, before `@<marketplace>` in its id.
pub const NAME: &str = "chloectx";
/// The marketplace this repository declares.
pub const MARKETPLACE: &str = "chloedb";
/// The prefix of the plugin's MCP tools (`mcp__plugin_<plugin>_<server>__`).
pub const TOOL_PREFIX: &str = "mcp__plugin_chloectx_chloectx__";
/// The variable the launcher sets for the program it runs.
pub const ENV: &str = "CHLOECTX_PLUGIN";
/// The launcher, in the plugin's folder.
pub const LAUNCHER: &str = "scripts/chloectx-launch";

/// Whether the plugin's launcher started this process.
pub fn called_by_plugin() -> bool {
    std::env::var(ENV).is_ok_and(|v| v == "1")
}

/// Claude Code's plugins folder: `$CLAUDE_CODE_PLUGIN_CACHE_DIR`, else
/// `plugins` beside the user settings (`~/.claude/plugins`).
pub fn plugins_root() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CODE_PLUGIN_CACHE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    crate::paths::user_settings().parent().map(|p| p.join("plugins")).unwrap_or_else(|| PathBuf::from("plugins"))
}

/// The plugin, as the settings of a project enable it.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// `chloectx@<marketplace>`.
    pub id: String,
    /// The settings file whose `enabledPlugins` turns it on.
    pub settings: PathBuf,
    /// Its installed folder, when `installed_plugins.json` has it.
    pub install_path: Option<PathBuf>,
    pub version: Option<String>,
}

impl Found {
    /// The launcher in the installed folder.
    pub fn launcher(&self) -> Option<PathBuf> {
        self.install_path.as_ref().map(|p| p.join(LAUNCHER))
    }

    /// The plugin's data folder (`${CLAUDE_PLUGIN_DATA}`), where the
    /// launcher keeps its setup's log and state.
    pub fn data_dir(&self, root: &Path) -> PathBuf {
        let id: String = self.id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
        root.join("data").join(id)
    }

    /// The hook events its `hooks/hooks.json` has.
    pub fn hook_events(&self) -> Vec<String> {
        let Some(dir) = &self.install_path else { return Vec::new() };
        let v = read_json(&dir.join("hooks/hooks.json"));
        v.get("hooks").and_then(|h| h.as_object()).map(|h| h.keys().cloned().collect()).unwrap_or_default()
    }
}

fn read_json(path: &Path) -> Json {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null)
}

/// The plugin as Claude Code would load it in `project`, from the user
/// settings, the project's `.claude/settings.json` and its
/// `.claude/settings.local.json`, in that order of precedence: for each id
/// `chloectx@…`, the last of them that names it decides.
pub fn enabled(project: &Path) -> Option<Found> {
    enabled_in(&crate::paths::user_settings(), project, &plugins_root())
}

/// [`enabled`] with the user settings and plugins folder given.
pub fn enabled_in(user_settings: &Path, project: &Path, plugins: &Path) -> Option<Found> {
    let mut files = vec![user_settings.to_path_buf()];
    let user = user_settings.canonicalize().ok();
    for f in ["settings.json", "settings.local.json"] {
        let p = project.join(".claude").join(f);
        // A session in the home folder: its project settings are the user's.
        if p.canonicalize().ok().map_or(true, |c| Some(c) != user) {
            files.push(p);
        }
    }
    let mut state: Vec<(String, bool, PathBuf)> = Vec::new();
    for f in &files {
        let v = read_json(f);
        let Some(map) = v.get("enabledPlugins").and_then(|e| e.as_object()) else { continue };
        for (id, on) in map {
            if id.split('@').next() != Some(NAME) || !id.contains('@') {
                continue;
            }
            let on = on.as_bool().unwrap_or(false);
            state.retain(|(i, _, _)| i != id);
            state.push((id.clone(), on, f.clone()));
        }
    }
    let (id, _, settings) = state.into_iter().rev().find(|(_, on, _)| *on)?;
    let (install_path, version) = installed(plugins, &id, project);
    Some(Found { id, settings, install_path, version })
}

/// Where `installed_plugins.json` says the plugin `id` is installed for
/// `project`: an install of the project's own scope first, else the user's.
fn installed(plugins: &Path, id: &str, project: &Path) -> (Option<PathBuf>, Option<String>) {
    let v = read_json(&plugins.join("installed_plugins.json"));
    let Some(list) = v.pointer(&format!("/plugins/{}", id.replace('~', "~0").replace('/', "~1"))).and_then(|l| l.as_array()) else { return (None, None) };
    let here = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
    let mine = |e: &&Json| e.get("projectPath").and_then(|p| p.as_str()).is_some_and(|p| Path::new(p).canonicalize().unwrap_or_else(|_| PathBuf::from(p)) == here);
    let entry = list.iter().find(mine).or_else(|| list.iter().find(|e| e.get("scope").and_then(|s| s.as_str()) == Some("user"))).or(list.first());
    let Some(e) = entry else { return (None, None) };
    (e.get("installPath").and_then(|p| p.as_str()).map(PathBuf::from), e.get("version").and_then(|v| v.as_str()).map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(p: &Path, v: Json) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, v.to_string()).unwrap();
    }

    #[test]
    fn the_most_local_settings_decide_and_the_install_is_found() {
        let t = tempfile::tempdir().unwrap();
        let user = t.path().join("home/.claude/settings.json");
        let plugins = t.path().join("home/.claude/plugins");
        let project = t.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(enabled_in(&user, &project, &plugins), None);

        write(&user, json!({"enabledPlugins": {"chloectx@chloedb": true, "other@x": true}}));
        let f = enabled_in(&user, &project, &plugins).unwrap();
        assert_eq!((f.id.as_str(), f.settings.as_path(), f.install_path.as_ref()), ("chloectx@chloedb", user.as_path(), None));

        // The project turns it off: off here.
        let shared = project.join(".claude/settings.json");
        write(&shared, json!({"enabledPlugins": {"chloectx@chloedb": false}}));
        assert_eq!(enabled_in(&user, &project, &plugins), None);
        // Its local settings turn it on again.
        let local = project.join(".claude/settings.local.json");
        write(&local, json!({"enabledPlugins": {"chloectx@chloedb": true}}));
        assert_eq!(enabled_in(&user, &project, &plugins).unwrap().settings, local);
        // A plugin of another name is not it, whatever its marketplace.
        write(&local, json!({"enabledPlugins": {"chloectx-extra@chloedb": true, "chloectx": true}}));
        assert_eq!(enabled_in(&user, &project, &plugins), None);

        // Installed for the user and for this project: the project's install.
        write(&local, json!({"enabledPlugins": {"chloectx@fork": true}}));
        write(&plugins.join("installed_plugins.json"), json!({"version": 2, "plugins": {"chloectx@fork": [
            {"scope": "user", "installPath": "/u/0.3.1", "version": "0.3.1"},
            {"scope": "local", "installPath": "/p/0.3.2", "version": "0.3.2", "projectPath": project.to_string_lossy()},
        ]}}));
        let f = enabled_in(&user, &project, &plugins).unwrap();
        assert_eq!(f.id, "chloectx@fork");
        assert_eq!(f.install_path.as_deref(), Some(Path::new("/p/0.3.2")));
        assert_eq!(f.version.as_deref(), Some("0.3.2"));
        assert_eq!(f.launcher().unwrap(), Path::new("/p/0.3.2/scripts/chloectx-launch"));
        assert_eq!(f.data_dir(&plugins), plugins.join("data/chloectx-fork"));
        // Another project gets the user's.
        let other = t.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        write(&user, json!({"enabledPlugins": {"chloectx@fork": true}}));
        assert_eq!(enabled_in(&user, &other, &plugins).unwrap().install_path.as_deref(), Some(Path::new("/u/0.3.1")));
    }

    /// The plugin in this repository: its hooks are the ones `chloectx
    /// install` writes, through the launcher; its version is the crate's,
    /// the release whose chloectx the launcher downloads; the marketplace
    /// lists it under the names the docs give.
    #[test]
    fn the_plugin_matches_the_install_and_the_marketplace() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin");
        let manifest = read_json(&dir.join(".claude-plugin/plugin.json"));
        assert_eq!(manifest["name"], NAME);
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"), "plugin.json's version names the release the launcher downloads");

        let hooks = read_json(&dir.join("hooks/hooks.json"));
        let theirs = hooks["hooks"].as_object().unwrap();
        let ours = crate::install::hooks("/x/chloectx", true);
        let ours = ours.as_object().unwrap();
        assert_eq!(theirs.keys().collect::<Vec<_>>(), ours.keys().collect::<Vec<_>>());
        for (event, list) in ours {
            let (a, b) = (&theirs[event][0], &list[0]);
            assert_eq!(a.get("matcher"), b.get("matcher"), "{event}");
            assert_eq!(a["hooks"][0]["timeout"], b["hooks"][0]["timeout"], "{event}");
            assert_eq!(a["hooks"][0]["command"], format!("\"${{CLAUDE_PLUGIN_ROOT}}/{LAUNCHER}\" hook --user {event}"));
        }
        let mcp = read_json(&dir.join(".mcp.json"));
        assert_eq!(mcp["mcpServers"][crate::install::SERVER], json!({"command": format!("${{CLAUDE_PLUGIN_ROOT}}/{LAUNCHER}"), "args": ["mcp"]}));
        assert_eq!(TOOL_PREFIX, format!("mcp__plugin_{NAME}_{}__", crate::install::SERVER));

        let market = read_json(&dir.join("../../.claude-plugin/marketplace.json"));
        assert_eq!(market["name"], MARKETPLACE);
        let entry = market["plugins"].as_array().unwrap().iter().find(|p| p["name"] == NAME).unwrap();
        assert_eq!(entry["source"], "./chloectx/plugin");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(LAUNCHER)).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "the launcher is executable");
        }
    }
}
