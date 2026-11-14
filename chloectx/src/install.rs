//! `chloectx install`: wires ChloeCtx into Claude Code settings.
//!
//! For one project, hooks go to `.claude/settings.local.json` (personal, not
//! committed) by default, or `.claude/settings.json` with `--shared`. The MCP
//! server goes to `.mcp.json` and is pre-approved in the same settings file.
//!
//! For every session (`--user`), hooks and the permission go to the user
//! settings (`~/.claude/settings.json`) and the MCP server is registered at
//! user scope with `claude mcp add-json`. Those hooks run as
//! `chloectx hook --user <Event>` and step aside in a project that has its own
//! install, so nothing is recorded twice. Each project still gets its own
//! store, keyed by its directory.
//!
//! `chloectx uninstall` removes what `install` wrote and keeps the stores.
//!
//! ChloeCtx was called Engram. `install` and `uninstall` also replace or
//! remove what an install from before the rename wrote under its old names:
//! hooks that call the engram program, the MCP server named engram with the
//! permission for its tools, and the setting ENGRAM_DISTILL_MODEL (before the rename).
//! The server and the permission are taken for ChloeCtx's only where the
//! same settings hold hooks of the program the server runs, since other
//! tools are named Engram too.

use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};

use crate::{err, Result};

/// The hooks installed: event, matcher, timeout in seconds.
pub const EVENTS: [(&str, Option<&str>, u64); 11] = [
    ("SessionStart", None, 15),
    // Long enough for query understanding in `sync` mode.
    ("UserPromptSubmit", None, 90),
    ("PreToolUse", Some("Edit|MultiEdit|Write"), 5),
    ("PostToolUse", None, 10),
    ("PostToolUseFailure", None, 10),
    // Long enough to wait for a query-understanding search still running.
    ("Stop", None, 40),
    ("PreCompact", None, 15),
    ("PostCompact", None, 15),
    ("SessionEnd", None, 10),
    // Recorded for `chloectx report` (when the agent waited for the user,
    // when a sub-agent finished), never recalled.
    ("Notification", None, 5),
    ("SubagentStop", None, 5),
];

/// The MCP server's name; its tools are `mcp__chloectx__*`.
pub const SERVER: &str = "chloectx";
/// The server's name before the rename.
pub const OLD_SERVER: &str = "engram";
/// The setting that names the model distilling turns.
const DISTILL_MODEL: &str = "CHLOECTX_DISTILL_MODEL";

fn permission(server: &str) -> String {
    format!("mcp__{server}")
}

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

/// Writes `v` to `path` as pretty JSON, so that the file is either as it
/// was or as it is now, never cut short (a machine that sleeps or a disk
/// that fills mid-write would leave Claude Code with broken settings):
/// written to a file beside it, flushed to disk and renamed over it. A
/// link (a dotfiles manager's) stays a link: the file it points to is
/// replaced. Keys keep the order the file had them in ([`in_order`]),
/// new ones after, so the rewrite's diff is what changed.
fn write_json(path: &Path, v: &Json) -> Result<()> {
    use std::io::Write;
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let before = std::fs::read_to_string(&target).ok();
    let order = before.as_deref().and_then(|t| serde_json::from_str::<Order>(t).ok());
    let text = serde_json::to_string_pretty(&in_order(v, order.as_ref()))? + "\n";
    let name = target.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let tmp = dir.join(format!(".{name}.chloectx-{}.tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        if let Ok(m) = std::fs::metadata(&target) {
            f.set_permissions(m.permissions())?;
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)?;
        // The rename itself, on disk (best effort: not every platform
        // opens a directory).
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

/// The order of the keys of a JSON text's objects, nested as its values
/// are (`serde_json`'s own map sorts them).
#[derive(Default)]
struct Order {
    keys: Vec<(String, Order)>,
    items: Vec<Order>,
}

impl<'de> serde::Deserialize<'de> for Order {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Order, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Order;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<Order, E> {
                Ok(Order::default())
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<Order, E> {
                Ok(Order::default())
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<Order, E> {
                Ok(Order::default())
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<Order, E> {
                Ok(Order::default())
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<Order, E> {
                Ok(Order::default())
            }
            fn visit_unit<E>(self) -> std::result::Result<Order, E> {
                Ok(Order::default())
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> std::result::Result<Order, A::Error> {
                let mut items = Vec::new();
                while let Some(o) = a.next_element::<Order>()? {
                    items.push(o);
                }
                Ok(Order { keys: Vec::new(), items })
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut a: A) -> std::result::Result<Order, A::Error> {
                let mut keys = Vec::new();
                while let Some((k, o)) = a.next_entry::<String, Order>()? {
                    keys.push((k, o));
                }
                Ok(Order { keys, items: Vec::new() })
            }
        }
        d.deserialize_any(V)
    }
}

/// `v` serialised with each object's keys in `order`'s order, the keys
/// `order` lacks after them.
fn in_order<'a>(v: &'a Json, order: Option<&'a Order>) -> InOrder<'a> {
    InOrder(v, order)
}

struct InOrder<'a>(&'a Json, Option<&'a Order>);

impl serde::Serialize for InOrder<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self.0 {
            Json::Object(m) => {
                let mut out = s.serialize_map(Some(m.len()))?;
                let known = self.1.map(|o| o.keys.as_slice()).unwrap_or(&[]);
                for (k, o) in known {
                    if let Some(v) = m.get(k) {
                        out.serialize_entry(k, &InOrder(v, Some(o)))?;
                    }
                }
                for (k, v) in m.iter().filter(|(k, _)| !known.iter().any(|(o, _)| o == *k)) {
                    out.serialize_entry(k, &InOrder(v, None))?;
                }
                out.end()
            }
            Json::Array(a) => {
                let mut out = s.serialize_seq(Some(a.len()))?;
                for (i, v) in a.iter().enumerate() {
                    out.serialize_element(&InOrder(v, self.1.and_then(|o| o.items.get(i))))?;
                }
                out.end()
            }
            v => serde::Serialize::serialize(v, s),
        }
    }
}

/// Copies `path` to `path.chloectx-<time>.bak` before it is rewritten.
fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".chloectx-{}.bak", crate::now()));
    let to = path.with_file_name(name);
    std::fs::copy(path, &to)?;
    Ok(Some(to))
}

/// [`backup`], when the file exists and writing `v` would change what it holds.
fn backup_if_changed(path: &Path, v: &Json) -> Result<Option<PathBuf>> {
    match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Json>(&t).ok()) {
        Some(old) if &old == v => Ok(None),
        _ => backup(path),
    }
}

/// The line that names the backups a command made.
fn saved_note(saved: &[PathBuf]) -> String {
    match saved {
        [] => String::new(),
        s => format!("\n  previous versions saved as {}", s.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")),
    }
}

/// The commands of a hooks entry.
fn entry_commands(entry: &Json) -> impl Iterator<Item = &str> {
    entry.get("hooks").and_then(|h| h.as_array()).into_iter().flatten().filter_map(|h| h.get("command").and_then(|c| c.as_str()))
}

/// `s` as one word of a POSIX shell command: as it is when it holds only
/// characters no shell treats specially, else in single quotes (Claude
/// Code runs a hook's command with a shell, so `/Users/jo/My Tools/bin/chloectx`
/// unquoted would run `/Users/jo/My`).
pub fn shell_word(s: &str) -> String {
    let plain = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:@,%".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The first word of a shell command and the rest after it: a single- or
/// double-quoted word is unquoted (the quoting [`shell_word`] writes, and
/// a double-quoted path without escapes); an unquoted one runs to the first ` hook `,
/// as installs before the quoting wrote paths with spaces.
fn first_word(command: &str) -> Option<(String, &str)> {
    let c = command.trim_start();
    let quote = c.chars().next().filter(|q| *q == '\'' || *q == '"');
    let Some(q) = quote else {
        let (exe, _) = c.split_once(" hook ")?;
        return Some((exe.trim().to_string(), &c[exe.len()..]));
    };
    let mut word = String::new();
    let mut rest = &c[1..];
    loop {
        let end = rest.find(q)?;
        let part = &rest[..end];
        rest = &rest[end + 1..];
        word.push_str(part);
        // `'\''` continues a single-quoted word with a quote.
        if q == '\'' && rest.starts_with("\\''") {
            word.push('\'');
            rest = &rest[3..];
            continue;
        }
        if rest.is_empty() || rest.starts_with(' ') {
            return Some((word, rest));
        }
        return None;
    }
}

/// The program a ChloeCtx hook command calls: `<exe> hook [--user] <Event>`,
/// where the program is chloectx, or engram (its name before the rename),
/// and `<exe>` may be quoted.
pub fn hook_exe(command: &str) -> Option<String> {
    let (exe, rest) = first_word(command)?;
    if !rest.starts_with(" hook ") {
        return None;
    }
    let name = Path::new(&exe).file_name()?.to_str()?.to_string();
    (name.contains(SERVER) || name.contains(OLD_SERVER)).then_some(exe)
}

/// The shell text of a hook command's program, as written (quotes and
/// all): what `doctor` runs to see that a shell finds the program.
pub fn hook_exe_text(command: &str) -> Option<&str> {
    let c = command.trim_start();
    let (_, rest) = first_word(c)?;
    rest.starts_with(" hook ").then(|| &c[..c.len() - rest.len()])
}

/// Whether a hooks entry calls ChloeCtx.
fn is_own_hook(entry: &Json) -> bool {
    entry_commands(entry).any(|c| hook_exe(c).is_some())
}

/// Whether a hooks entry is a user-level ChloeCtx hook.
fn is_user_hook(entry: &Json) -> bool {
    entry_commands(entry).any(|c| hook_exe(c).is_some() && c.contains(" hook --user "))
}

/// The programs that the ChloeCtx hooks of a settings object call under the
/// old name (`.../engram`): an install from before the rename.
fn old_exes(settings: &Json) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for list in settings.get("hooks").and_then(|h| h.as_object()).into_iter().flat_map(|h| h.values()) {
        for c in list.as_array().into_iter().flatten().flat_map(entry_commands) {
            if let Some(exe) = hook_exe(c).filter(|e| !Path::new(e).file_name().map_or(false, |n| n.to_string_lossy().contains(SERVER))) {
                if !out.iter().any(|o| *o == exe) {
                    out.push(exe);
                }
            }
        }
    }
    out
}

/// Whether an MCP server entry is ChloeCtx's from before the rename: it
/// runs `<exe> mcp` for a program the old hooks call.
fn is_old_server(server: Option<&Json>, old_exes: &[String]) -> bool {
    let Some(s) = server else { return false };
    let cmd = s.get("command").and_then(|c| c.as_str()).unwrap_or("");
    s.get("args") == Some(&json!(["mcp"])) && old_exes.iter().any(|e| e == cmd)
}

/// Removes the entries of an install from before the rename that are not
/// hooks (those go with ChloeCtx's own): the `chloectx` MCP approval and
/// permission, when the settings hold the old hooks, and the old setting.
fn remove_old_names(settings: &mut Json, had_old_hooks: bool) {
    if had_old_hooks {
        remove_item(settings, "enabledMcpjsonServers", OLD_SERVER);
        if let Some(p) = settings.get_mut("permissions").filter(|p| p.is_object()) {
            remove_item(p, "allow", &permission(OLD_SERVER));
        }
    }
    if let Some(env) = settings.get_mut("env").and_then(|e| e.as_object_mut()) {
        let old = crate::env::old_name(DISTILL_MODEL).unwrap();
        if let Some(v) = env.remove(&old) {
            env.entry(DISTILL_MODEL).or_insert(v);
        }
    }
}

/// The hooks block for `exe` (merged into an existing settings object).
pub fn hooks(exe: &str, user: bool) -> Json {
    let flag = if user { " --user" } else { "" };
    let mut h = serde_json::Map::new();
    for (event, matcher, timeout) in EVENTS {
        let mut entry = json!({"hooks": [{"type": "command", "command": format!("{} hook{flag} {event}", shell_word(exe)), "timeout": timeout}]});
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

/// Adds ChloeCtx's hooks, the memory tools' permission and (for a project) the
/// MCP approval to a settings object, replacing earlier ChloeCtx entries.
pub fn add_to_settings(settings: &mut Json, exe: &str, user: bool, distill_model: Option<&str>) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let had_old_hooks = !old_exes(settings).is_empty();
    let mut existing = settings.get("hooks").cloned().filter(|h| h.is_object()).unwrap_or_else(|| json!({}));
    for (event, entries) in hooks(exe, user).as_object().unwrap() {
        let list = existing.get(event).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        // Drop earlier ChloeCtx entries, keep everything else.
        let mut kept: Vec<Json> = list.into_iter().filter(|e| !is_own_hook(e)).collect();
        kept.extend(entries.as_array().unwrap().iter().cloned());
        existing[event] = Json::Array(kept);
    }
    settings["hooks"] = existing;
    remove_old_names(settings, had_old_hooks);
    if !user {
        push_unique(settings, "enabledMcpjsonServers", SERVER);
    }
    // The memory tools read and write only ChloeCtx's own store.
    if !settings.get("permissions").map_or(false, |p| p.is_object()) {
        settings["permissions"] = json!({});
    }
    push_unique(&mut settings["permissions"], "allow", &permission(SERVER));
    if let Some(m) = distill_model.filter(|m| !m.is_empty()) {
        if !settings.get("env").map_or(false, |e| e.is_object()) {
            settings["env"] = json!({});
        }
        settings["env"][DISTILL_MODEL] = json!(m);
    }
}

/// Removes everything [`add_to_settings`] adds. Returns whether anything changed.
pub fn remove_from_settings(settings: &mut Json) -> bool {
    let before = settings.clone();
    let had_old_hooks = !old_exes(settings).is_empty();
    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for list in hooks.values_mut() {
            if let Some(l) = list.as_array_mut() {
                l.retain(|e| !is_own_hook(e));
            }
        }
        hooks.retain(|_, l| l.as_array().map_or(true, |l| !l.is_empty()));
        if hooks.is_empty() {
            settings.as_object_mut().unwrap().remove("hooks");
        }
    }
    if settings.is_object() {
        remove_item(settings, "enabledMcpjsonServers", SERVER);
        remove_old_names(settings, had_old_hooks);
    }
    if let Some(p) = settings.get_mut("permissions").filter(|p| p.is_object()) {
        remove_item(p, "allow", &permission(SERVER));
    }
    if let Some(env) = settings.get_mut("env").and_then(|e| e.as_object_mut()) {
        env.remove(DISTILL_MODEL);
    }
    if let Some(obj) = settings.as_object_mut() {
        obj.retain(|k, v| !((k == "permissions" || k == "env") && v.as_object().map_or(false, |o| o.is_empty())));
    }
    *settings != before
}

/// Whether the project's own settings install ChloeCtx's hooks. The user
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
        hooks.values().filter_map(|l| l.as_array()).flatten().any(|e| is_own_hook(e) && !is_user_hook(e))
    })
}

/// Whether the user settings hold the hooks of `chloectx install --user`.
pub fn user_has_hooks() -> bool {
    let settings = read_json(&crate::paths::user_settings());
    let Some(hooks) = settings.get("hooks").and_then(|h| h.as_object()) else { return false };
    hooks.values().filter_map(|l| l.as_array()).flatten().any(is_user_hook)
}

/// What a program that starts Claude Code sessions of its own (Workbench)
/// adds to each session so that ChloeCtx records it and its agent has the
/// memory tools, without writing any settings file of the user's:
/// `settings` (merge into the session's `--settings` file) and `mcp` (into
/// its `--mcp-config`). Both are empty when `cwd` is already wired, by the
/// user's settings (`wired: "user"`) or its project's (`"project"`), so a
/// session is never recorded twice.
pub fn wiring(cwd: &Path, exe: &str, distill_model: Option<&str>) -> Json {
    let wired = if project_has_hooks(cwd) {
        Some("project")
    } else if user_has_hooks() {
        Some("user")
    } else {
        None
    };
    let (settings, mcp) = if wired.is_some() {
        (json!({}), json!({}))
    } else {
        let mut s = json!({});
        add_to_settings(&mut s, exe, false, distill_model);
        // The server comes from --mcp-config, not a project's .mcp.json.
        if let Some(o) = s.as_object_mut() {
            o.remove("enabledMcpjsonServers");
        }
        (s, json!({ SERVER: {"type": "stdio", "command": exe, "args": ["mcp"]} }))
    };
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "exe": exe,
        "wired": wired,
        "settings": settings,
        "mcp": mcp,
        "store": crate::paths::store_dir(cwd).to_string_lossy(),
        "model": model_status(),
    })
}

/// Whether recall's embedding model is there (`chloectx fetch-model`
/// downloads it); another embedder set in the environment needs none.
fn model_status() -> Json {
    let spec = std::env::var("CHLOECTX_EMBEDDER").unwrap_or_else(|_| "static".into());
    let dir = crate::embed::models_dir().join("potion-retrieval-32M");
    let ok = spec != "static" || dir.join("model.safetensors").is_file();
    json!({"ok": ok, "embedder": spec, "path": dir.to_string_lossy(), "fix": if ok { Json::Null } else { json!("chloectx fetch-model") }})
}

/// The nearest folder above `p` that exists, for a mistyped path.
fn nearest_existing(p: &Path) -> Option<&Path> {
    p.ancestors().skip(1).find(|a| a.is_dir() && a.parent().is_some())
}

/// In a git work tree, keeps this machine's install files out of `git add`:
/// each of `files` that is neither tracked nor ignored yet goes into the
/// repository's `info/exclude` (local, never committed). What it did or
/// what to watch for, for the person (CTX-12).
fn keep_out_of_git(project: &Path, files: &[&str]) -> Vec<String> {
    let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(project).args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok();
    let Some(out) = git(&["rev-parse", "--git-path", "info/exclude"]).filter(|o| o.status.success()) else { return Vec::new() };
    let exclude = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let exclude = if exclude.is_absolute() { exclude } else { project.join(exclude) };
    let mut notes = Vec::new();
    let mut add = Vec::new();
    for f in files {
        if git(&["ls-files", "--error-unmatch", "--", f]).is_some_and(|o| o.status.success()) {
            notes.push(format!("{f} is tracked by git and now names this machine's chloectx; do not commit that change"));
        } else if !git(&["check-ignore", "-q", "--", f]).is_some_and(|o| o.status.success()) {
            add.push(format!("/{f}"));
        }
    }
    if !add.is_empty() {
        let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text += "# ChloeCtx's project install: paths of this machine\n";
        for a in &add {
            text += &format!("{a}\n");
        }
        let ok = exclude.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&exclude, text).is_ok();
        if ok {
            notes.push(format!("{} hold this machine's paths, so they were added to {} (git leaves them out)", add.iter().map(|a| a.trim_start_matches('/')).collect::<Vec<_>>().join(" and "), exclude.display()));
        } else {
            notes.push(format!("{} hold this machine's paths: do not commit them", add.iter().map(|a| a.trim_start_matches('/')).collect::<Vec<_>>().join(" and ")));
        }
    }
    notes
}

pub fn install(project: &Path, exe: &str, shared: bool, distill_model: Option<&str>) -> Result<String> {
    // A mistyped path is not created (CTX-9).
    if !project.is_dir() {
        let hint = nearest_existing(project).map(|a| format!(" (the nearest folder above it that exists is {})", a.display())).unwrap_or_default();
        return Err(err(format!("{} is not a folder{hint}; give the project's folder, as `chloectx --project DIR install`", project.display())));
    }
    let settings_path = project.join(".claude").join(if shared { "settings.json" } else { "settings.local.json" });
    let mut settings = read_settings(&settings_path)?;
    let mcp_path = project.join(".mcp.json");
    let mut mcp = read_settings(&mcp_path)?;
    let old = project_old_exes(project);
    add_to_settings(&mut settings, exe, false, distill_model);
    let mut saved = Vec::new();
    saved.extend(backup_if_changed(&settings_path, &settings)?);
    write_json(&settings_path, &settings)?;

    if !mcp.is_object() {
        mcp = json!({});
    }
    if mcp.get("mcpServers").map_or(true, |v| !v.is_object()) {
        mcp["mcpServers"] = json!({});
    }
    let servers = mcp["mcpServers"].as_object_mut().unwrap();
    if is_old_server(servers.get(OLD_SERVER), &old) {
        servers.remove(OLD_SERVER);
    }
    servers.insert(SERVER.into(), json!({"command": exe, "args": ["mcp"]}));
    saved.extend(backup_if_changed(&mcp_path, &mcp)?);
    write_json(&mcp_path, &mcp)?;
    // Shared settings are meant to be committed; the local ones are not.
    let git = if shared { Vec::new() } else { keep_out_of_git(project, &[".claude/settings.local.json", ".mcp.json"]) };
    Ok(format!(
        "ChloeCtx installed for {}:\n  hooks and MCP approval: {}\n  MCP server: {}\n  store: {}{}{}",
        project.display(),
        settings_path.display(),
        mcp_path.display(),
        crate::paths::store_dir(project).display(),
        git.iter().map(|n| format!("\n  git: {n}")).collect::<String>(),
        saved_note(&saved)
    ))
}

/// The programs the old-named ChloeCtx hooks of a project's settings call.
fn project_old_exes(project: &Path) -> Vec<String> {
    ["settings.local.json", "settings.json"].iter().flat_map(|f| old_exes(&read_json(&project.join(".claude").join(f)))).collect()
}

/// Removes the user-scope `chloectx` MCP server when it is ChloeCtx's from
/// before the rename (it runs a program the old user hooks call).
fn remove_old_user_server(old: &[String]) -> Option<String> {
    let cfg = read_json(&crate::paths::claude_json());
    if !is_old_server(cfg.pointer(&format!("/mcpServers/{OLD_SERVER}")), old) {
        return None;
    }
    Some(match claude_mcp(&["remove", OLD_SERVER, "--scope", "user"]) {
        Ok(()) => format!("; the old `{OLD_SERVER}` one removed"),
        Err(e) => format!("; the old `{OLD_SERVER}` one is still registered ({e}): claude mcp remove {OLD_SERVER} --scope user"),
    })
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

/// Installs ChloeCtx for every Claude Code session of this user.
pub fn install_user(exe: &str, distill_model: Option<&str>) -> Result<String> {
    let settings_path = crate::paths::user_settings();
    let mut settings = read_settings(&settings_path)?;
    let old = old_exes(&settings);
    add_to_settings(&mut settings, exe, true, distill_model);

    // Before the hooks are written: `claude mcp` runs them too.
    let server = json!({"type": "stdio", "command": exe, "args": ["mcp"]}).to_string();
    let _ = claude_mcp(&["remove", SERVER, "--scope", "user"]);
    let mut mcp = match claude_mcp(&["add-json", SERVER, &server, "--scope", "user"]) {
        Ok(()) => format!("registered at user scope (claude mcp get {SERVER})"),
        Err(e) => format!("not registered ({e}); run: claude mcp add-json {SERVER} '{server}' --scope user"),
    };
    mcp += &remove_old_user_server(&old).unwrap_or_default();
    let saved = backup(&settings_path)?;
    write_json(&settings_path, &settings)?;
    Ok(format!(
        "ChloeCtx installed for every Claude Code session:\n  hooks and permission: {}{}\n  MCP server: {mcp}\n  stores: {}/<project>/memory.strata, one per project directory\n  \
         projects with their own `chloectx install` keep using it",
        settings_path.display(),
        saved.map(|p| format!(" (previous version saved as {})", p.display())).unwrap_or_default(),
        crate::paths::home().join("projects").display(),
    ))
}

/// Removes ChloeCtx from a project's settings and `.mcp.json`; keeps the store.
pub fn uninstall(project: &Path) -> Result<String> {
    let mut changed = Vec::new();
    let mut saved = Vec::new();
    let old = project_old_exes(project);
    for f in ["settings.local.json", "settings.json"] {
        let path = project.join(".claude").join(f);
        let mut settings = read_settings(&path)?;
        if path.exists() && remove_from_settings(&mut settings) {
            saved.extend(backup(&path)?);
            write_json(&path, &settings)?;
            changed.push(path);
        }
    }
    let mcp_path = project.join(".mcp.json");
    let mut mcp = read_json(&mcp_path);
    if let Some(servers) = mcp.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
        let old_server = is_old_server(servers.get(OLD_SERVER), &old) && servers.remove(OLD_SERVER).is_some();
        if servers.remove(SERVER).is_some() || old_server {
            saved.extend(backup(&mcp_path)?);
            write_json(&mcp_path, &mcp)?;
            changed.push(mcp_path);
        }
    }
    if changed.is_empty() {
        return Ok(format!("ChloeCtx was not installed in {}", project.display()));
    }
    Ok(format!(
        "ChloeCtx removed from {}\n  store kept: {}{}",
        changed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
        crate::paths::store_dir(project).display(),
        saved_note(&saved)
    ))
}

/// Removes the user-level install; keeps the stores.
pub fn uninstall_user() -> Result<String> {
    let settings_path = crate::paths::user_settings();
    let mut settings = read_settings(&settings_path)?;
    let old = old_exes(&settings);
    let hooks = if settings_path.exists() && remove_from_settings(&mut settings) {
        let saved = backup(&settings_path)?;
        write_json(&settings_path, &settings)?;
        format!("removed from {}{}", settings_path.display(), saved.map(|p| format!(" (previous version saved as {})", p.display())).unwrap_or_default())
    } else {
        format!("none in {}", settings_path.display())
    };
    let mut mcp = match claude_mcp(&["remove", SERVER, "--scope", "user"]) {
        Ok(()) => "removed".to_string(),
        Err(e) => format!("not removed ({e})"),
    };
    mcp += &remove_old_user_server(&old).unwrap_or_default();
    Ok(format!(
        "ChloeCtx uninstalled for every session:\n  hooks: {hooks}\n  MCP server: {mcp}\n  stores kept in {}",
        crate::paths::home().join("projects").display()
    ))
}

#[cfg(test)]
mod tests {

    #[test]
    fn wiring_skips_a_project_with_its_own_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let w = wiring(dir.path(), "/opt/chloectx", Some("haiku"));
        if !user_has_hooks() {
            assert_eq!(w["wired"], Json::Null, "{w}");
            assert!(w["settings"]["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap().contains("/opt/chloectx hook Stop"), "{w}");
            assert_eq!(w["settings"]["env"]["CHLOECTX_DISTILL_MODEL"], "haiku");
            assert!(w["settings"].get("enabledMcpjsonServers").is_none());
            assert_eq!(w["mcp"]["chloectx"]["args"], json!(["mcp"]));
        }
        install(dir.path(), "/opt/chloectx", false, None).unwrap();
        let w = wiring(dir.path(), "/opt/chloectx", Some("haiku"));
        assert_eq!(w["wired"], "project", "{w}");
        assert_eq!(w["settings"], json!({}));
        assert_eq!(w["mcp"], json!({}));
    }
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
        add_to_settings(&mut s, "/bin/chloectx", true, Some("haiku"));
        add_to_settings(&mut s, "/bin/chloectx", true, Some("haiku")); // idempotent
        let cmds = commands(&s);
        assert_eq!(cmds.iter().filter(|c| c.contains("chloectx hook --user ")).count(), EVENTS.len());
        assert!(cmds.contains(&"say done".to_string()));
        assert_eq!(s["permissions"]["allow"], json!(["Bash(ls:*)", "mcp__chloectx"]));
        assert_eq!(s["env"]["CHLOECTX_DISTILL_MODEL"], "haiku");
        assert!(s.get("enabledMcpjsonServers").is_none());
        assert!(remove_from_settings(&mut s));
        assert_eq!(s, original);
        assert!(!remove_from_settings(&mut s));
    }

    #[test]
    fn project_uninstall_removes_only_chloectx() {
        let mut s = json!({"enabledMcpjsonServers": ["other"]});
        add_to_settings(&mut s, "/x/chloectx", false, None);
        assert_eq!(s["enabledMcpjsonServers"], json!(["other", "chloectx"]));
        assert!(commands(&s).iter().all(|c| c.starts_with("/x/chloectx hook ") && !c.contains("--user")));
        remove_from_settings(&mut s);
        assert_eq!(s, json!({"enabledMcpjsonServers": ["other"]}));
    }

    /// Settings as an install from before the rename wrote them, with
    /// another tool's entries beside them.
    fn old_settings(user: bool) -> Json {
        let mut s = json!({
            "enabledMcpjsonServers": ["other"],
            "env": {"DISABLE_AUTOUPDATER": "1"},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]},
            "permissions": {"allow": ["Bash(ls:*)"]}
        });
        add_to_settings(&mut s, "/old/engram", user, Some("haiku"));
        // What the old install wrote under its names.
        let text = s.to_string().replace("mcp__chloectx", "mcp__engram").replace("\"chloectx\"", "\"engram\"").replace(DISTILL_MODEL, &crate::env::old_name(DISTILL_MODEL).unwrap());
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn install_replaces_the_entries_of_an_install_from_before_the_rename() {
        for user in [true, false] {
            let mut s = old_settings(user);
            assert_eq!(old_exes(&s), vec!["/old/engram".to_string()]);
            assert_eq!(s["env"][crate::env::old_name(DISTILL_MODEL).unwrap()], "haiku");
            add_to_settings(&mut s, "/new/bin/chloectx", user, None);
            let cmds = commands(&s);
            assert_eq!(cmds.iter().filter(|c| c.starts_with("/new/bin/chloectx hook ")).count(), EVENTS.len());
            assert!(!cmds.iter().any(|c| c.contains("engram")), "{cmds:?}");
            assert!(cmds.contains(&"say done".to_string()));
            assert_eq!(s["permissions"]["allow"], json!(["Bash(ls:*)", "mcp__chloectx"]));
            // The model the old install chose stays, under the new name.
            assert_eq!(s["env"], json!({"DISABLE_AUTOUPDATER": "1", "CHLOECTX_DISTILL_MODEL": "haiku"}));
            let servers = if user { json!(["other"]) } else { json!(["other", "chloectx"]) };
            assert_eq!(s["enabledMcpjsonServers"], servers);
            assert!(old_exes(&s).is_empty());
            // And uninstall leaves the other tool's entries.
            let mut u = old_settings(user);
            assert!(remove_from_settings(&mut u));
            assert_eq!(u, json!({"enabledMcpjsonServers": ["other"], "env": {"DISABLE_AUTOUPDATER": "1"}, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}, "permissions": {"allow": ["Bash(ls:*)"]}}));
        }
    }

    #[test]
    fn another_tool_named_engram_is_left_alone() {
        // No old ChloeCtx hooks: a server and a permission named engram are another tool's.
        let mut s = json!({"enabledMcpjsonServers": ["engram"], "permissions": {"allow": ["mcp__engram"]}});
        add_to_settings(&mut s, "/bin/chloectx", false, None);
        assert_eq!(s["enabledMcpjsonServers"], json!(["engram", "chloectx"]));
        assert_eq!(s["permissions"]["allow"], json!(["mcp__engram", "mcp__chloectx"]));
        remove_from_settings(&mut s);
        assert_eq!(s, json!({"enabledMcpjsonServers": ["engram"], "permissions": {"allow": ["mcp__engram"]}}));

        let old = vec!["/old/engram".to_string()];
        assert!(is_old_server(Some(&json!({"command": "/old/engram", "args": ["mcp"]})), &old));
        assert!(!is_old_server(Some(&json!({"command": "/opt/other/engram", "args": ["mcp"]})), &old));
        assert!(!is_old_server(Some(&json!({"command": "/old/engram", "args": ["serve"]})), &old));
        assert!(!is_old_server(None, &old));
        assert_eq!(hook_exe("/a b/engram hook --user Stop").as_deref(), Some("/a b/engram"));
        assert_eq!(hook_exe("/bin/chloectx hook Stop").as_deref(), Some("/bin/chloectx"));
        assert_eq!(hook_exe("/bin/other hook Stop"), None);
        assert_eq!(hook_exe("say done"), None);
    }

    #[test]
    fn a_project_install_moves_the_old_server_in_mcp_json() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        write_json(&project.join(".claude/settings.local.json"), &old_settings(false)).unwrap();
        let other = json!({"command": "/opt/homebrew/bin/engram-other", "args": ["mcp"]});
        write_json(&project.join(".mcp.json"), &json!({"mcpServers": {"engram": {"command": "/old/engram", "args": ["mcp"]}, "other": other}})).unwrap();
        install(project, "/new/bin/chloectx", false, None).unwrap();
        assert_eq!(
            read_json(&project.join(".mcp.json")),
            json!({"mcpServers": {"chloectx": {"command": "/new/bin/chloectx", "args": ["mcp"]}, "other": other}})
        );
        assert!(project_has_hooks(project));
        uninstall(project).unwrap();
        assert_eq!(read_json(&project.join(".mcp.json")), json!({"mcpServers": {"other": other}}));
        assert!(!project_has_hooks(project));
    }

    #[test]
    fn user_hooks_step_aside_only_for_a_project_install() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        assert!(!project_has_hooks(project));
        let local = project.join(".claude/settings.local.json");
        let mut s = json!({});
        add_to_settings(&mut s, "/bin/chloectx", true, None);
        write_json(&local, &s).unwrap();
        assert!(!project_has_hooks(project), "user-level hooks copied into a project are not a project install");
        install(project, "/bin/chloectx", false, None).unwrap();
        assert!(project_has_hooks(project));
        uninstall(project).unwrap();
        assert!(!project_has_hooks(project));
        assert_eq!(read_json(&project.join(".mcp.json")), json!({"mcpServers": {}}));
    }
}
