//! One-shot calls to a model through the `claude` CLI in print mode, shared
//! by distillation, consolidation and the query engine.
//!
//! The call runs without tools, from a scratch directory and with ChloeCtx
//! disabled, so it records nothing itself. The scratch directory is
//! `<home>/distill` ([`scratch_dir`]), which only this user may open: the
//! CLI loads project settings (hooks among them) and `CLAUDE.md` from the
//! directory it runs in, so a shared one such as `/tmp/chloectx-distill`
//! would let another local user run commands as this one, or steer every
//! note the distiller writes. With `isolated` it also skips the
//! user's settings and MCP servers: their plugins, hooks and tool definitions
//! would add tens of thousands of tokens to every call (and another memory
//! plugin would record it).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use crate::{err, Result};

/// How to reach the model.
#[derive(Clone, Debug)]
pub struct Cli<'a> {
    /// The CLI to call (`claude`).
    pub command: &'a str,
    /// Its `--model` argument; empty: the CLI's default.
    pub model: &'a str,
    pub timeout: Duration,
    pub isolated: bool,
}

/// Tokens a call read and wrote, as the CLI reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl Usage {
    fn of(v: &Json) -> Usage {
        let n = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        Usage { input: n("input_tokens"), cache_read: n("cache_read_input_tokens"), cache_write: n("cache_creation_input_tokens"), output: n("output_tokens") }
    }

    pub fn add(&mut self, o: Usage) {
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
    }

    /// For event logs.
    pub fn json(&self) -> Json {
        serde_json::json!({"input": self.input, "cache_read": self.cache_read, "cache_write": self.cache_write, "output": self.output})
    }
}

/// The model's reply and what the call cost.
#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub text: String,
    pub cost_usd: f64,
    pub usage: Usage,
    pub ms: u64,
}

/// The directory model calls run in: `distill` in ChloeCtx's home, made
/// private to this user ([`crate::paths::ensure_private_dir`]); an error
/// when it cannot be, rather than a call from a directory someone else can
/// write to.
pub fn scratch_dir() -> Result<PathBuf> {
    let home = crate::paths::home();
    crate::paths::create_private_dir(&home)?;
    let dir = home.join("distill");
    crate::paths::ensure_private_dir(&dir).map_err(|e| err(format!("the model's scratch directory: {e}")))?;
    Ok(dir)
}

/// Where Claude Code's hooks found the CLI on their own PATH, last: a
/// daemon started by a process without it on PATH (a shell, an editor's
/// MCP server) still finds it (CTX-3).
static HOOK_CLI: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Remembers where a hook found the CLI ([`resolve_cli`] tries it after
/// `CHLOECTX_CLAUDE`).
pub fn set_hook_cli(path: &std::path::Path) {
    if path.is_absolute() && is_executable(path) {
        *HOOK_CLI.lock().unwrap() = Some(path.to_path_buf());
    }
}

fn is_executable(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// `name` on this process's PATH.
pub fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| is_executable(p))
}

/// Where the CLI is usually installed, for a process whose PATH lacks it.
fn usual_places(name: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) {
        for d in [".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin", ".volta/bin", "bin"] {
            v.push(h.join(d).join(name));
        }
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        v.push(PathBuf::from(d).join(name));
    }
    v
}

/// The program to run for `command`: a path as it is; for a bare name,
/// `CHLOECTX_CLAUDE` (for `claude`), where the hooks found it, this
/// process's PATH, then the usual install places. The daemon inherits the
/// environment of whichever process started it, so its PATH alone is not
/// enough (CTX-3).
pub fn resolve_cli(command: &str) -> std::result::Result<PathBuf, String> {
    if command.contains('/') {
        return Ok(PathBuf::from(command));
    }
    if command == "claude" {
        if let Some(p) = std::env::var_os("CHLOECTX_CLAUDE").filter(|p| !p.is_empty()).map(PathBuf::from) {
            return if is_executable(&p) { Ok(p) } else { Err(format!("CHLOECTX_CLAUDE is {}, which is not an executable file", p.display())) };
        }
    }
    let hint = HOOK_CLI.lock().unwrap().clone().filter(|p| p.file_name().is_some_and(|n| n == command) && is_executable(p));
    if let Some(p) = hint.or_else(|| on_path(command)).or_else(|| usual_places(command).into_iter().find(|p| is_executable(p))) {
        return Ok(p);
    }
    Err(format!(
        "`{command}` was not found on this process's PATH nor in the usual places (~/.local/bin, ~/.claude/local, ~/.npm-global/bin, ~/.bun/bin, ~/.volta/bin, ~/bin, /opt/homebrew/bin, /usr/local/bin, /usr/bin); set CHLOECTX_CLAUDE to its path, or restart the daemon (`chloectx stop`) so that the next Claude Code hook starts it"
    ))
}

/// Runs the model once on `prompt` with `system` as its system prompt.
pub fn complete(cli: &Cli, system: &str, prompt: &str) -> Result<Reply> {
    let t0 = Instant::now();
    let scratch = scratch_dir()?;
    let exe = resolve_cli(cli.command).map_err(err)?;
    let mut cmd = Command::new(&exe);
    cmd.args(["-p", "--system-prompt", system, "--tools", "", "--no-session-persistence", "--output-format", "json"]);
    if cli.isolated {
        cmd.args(["--setting-sources", "project", "--strict-mcp-config"]);
    }
    if !cli.model.is_empty() {
        cmd.args(["--model", cli.model]);
    }
    let mut child = cmd
        .current_dir(&scratch)
        .env("CHLOECTX_DISABLE", "1")
        .env("ENGRAM_DISABLE", "1") // read by the hooks of a binary from before the rename
        // Cloud sessions pass their memory stores on to child CLIs: in a
        // distill or query engine call they are ~7,000 tokens of the user's
        // notes the call does not need.
        .env_remove("CLAUDE_MEMORY_STORES")
        .env_remove("CLAUDE_COWORK_MEMORY_PATH_OVERRIDE")
        // Every call's prompt is new, so the CLI's cache writes are never
        // read back; they are billed above plain input (half the cost of a
        // 15,000-token call).
        .env("DISABLE_PROMPT_CACHING", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| err(format!("{}: {e}", cli.command)))?;
    // The prompt is written, and the answer read, on threads of their own
    // while this one watches the clock: a reply larger than the pipe's
    // buffer (64 KB on Linux) would otherwise block the child on its write
    // until the timeout killed it.
    let mut stdin = child.stdin.take().unwrap();
    let input = prompt.as_bytes().to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    // Its stderr, read the same way: what the CLI says when it fails
    // (an unknown flag, a crash, a proxy error) is the cause (CTX-2).
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let stderr_tail = |r: std::thread::JoinHandle<Vec<u8>>| -> String {
        if !r.is_finished() {
            return String::new();
        }
        tail_text(&r.join().unwrap_or_default(), STDERR_KEPT)
    };
    let deadline = Instant::now() + cli.timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err("model call timed out"));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = writer.join();
    // The child has exited; its stdout closes unless a process it started
    // still holds it, which the deadline bounds as well.
    while !reader.is_finished() && Instant::now() <= deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !reader.is_finished() {
        return Err(err("model call timed out"));
    }
    let stdout = reader.join().map_err(|_| err("model output: the reader failed"))??;
    while !err_reader.is_finished() && Instant::now() <= deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let v: Json = match serde_json::from_slice(&stdout) {
        Ok(v) => v,
        Err(e) => {
            let said = stderr_tail(err_reader);
            let out = tail_text(&stdout, 300);
            let what = if !said.is_empty() { said } else if !out.is_empty() { format!("stdout: {out}") } else { format!("no output ({e})") };
            return Err(err(format!("{} exited with {}: {what}", cli.command, exit_text(&status))));
        }
    };
    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Err(err(format!("model call failed: {}", v.get("result").and_then(|r| r.as_str()).unwrap_or(""))));
    }
    Ok(Reply {
        text: v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string(),
        cost_usd: v.get("total_cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0),
        usage: v.get("usage").map(Usage::of).unwrap_or_default(),
        ms: t0.elapsed().as_millis() as u64,
    })
}

/// How much of the CLI's stderr a failed call keeps in its error.
const STDERR_KEPT: usize = 1500;

/// The last `max` bytes of `bytes` as text, at a character boundary, its
/// lines joined, prefixed with `…` when cut.
fn tail_text(bytes: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    let one: String = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" | ");
    if one.len() <= max {
        return one;
    }
    let mut i = one.len() - max;
    while !one.is_char_boundary(i) {
        i += 1;
    }
    format!("…{}", &one[i..])
}

fn exit_text(st: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            return format!("signal {sig}");
        }
    }
    match st.code() {
        Some(c) => format!("status {c}"),
        None => "an unknown status".into(),
    }
}

/// Runs the model once through Anthropic's Messages API with `key`
/// (`ANTHROPIC_BASE_URL` overrides where it is sent): for a process with no
/// `claude` CLI, such as `chloectx app` in a cluster.
pub fn complete_api(key: &str, model: &str, system: &str, prompt: &str, timeout: Duration) -> Result<Reply> {
    let t0 = Instant::now();
    let base = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).build().into();
    let body = serde_json::json!({
        "model": model, "max_tokens": 4096, "system": system,
        "messages": [{"role": "user", "content": prompt}],
    });
    let mut r = agent
        .post(format!("{}/v1/messages", base.trim_end_matches('/')))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .send(body.to_string())
        .map_err(|e| err(format!("model call: {e}")))?;
    let status = r.status().as_u16();
    let text = r.body_mut().read_to_string().map_err(|e| err(format!("model call: {e}")))?;
    let v: Json = serde_json::from_str(&text).unwrap_or(Json::Null);
    if !(200..300).contains(&status) {
        let m = v["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| text.chars().take(300).collect());
        return Err(err(format!("model call failed (HTTP {status}): {m}")));
    }
    let out: String = v["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("");
    Ok(Reply { text: out, cost_usd: 0.0, usage: Usage::of(&v["usage"]), ms: t0.elapsed().as_millis() as u64 })
}

/// The first JSON object in `text` (models sometimes wrap it in prose or a
/// code fence).
pub fn json_object(text: &str) -> Option<Json> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok().or_else(|| {
        // A trailing object after the first: take the longest prefix that parses.
        let mut de = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Json>();
        de.next().and_then(|r| r.ok())
    })
}

/// `s` cut to at most `n` bytes at a character boundary, with an ellipsis.
pub fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut i = n;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &s[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_object() {
        assert_eq!(json_object("Sure:\n```json\n{\"a\": 1}\n```").unwrap()["a"], 1);
        assert_eq!(json_object("{\"a\": {\"b\": 2}} and {\"c\": 3}").unwrap()["a"]["b"], 2);
        assert!(json_object("no json").is_none());
        assert_eq!(clip("héllo", 2), "h…");
    }

    #[test]
    fn reads_usage() {
        let v: Json = serde_json::from_str(r#"{"input_tokens": 3, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 50, "output_tokens": 120}"#).unwrap();
        let mut u = Usage::of(&v);
        assert_eq!(u, Usage { input: 3, cache_read: 1000, cache_write: 50, output: 120 });
        u.add(Usage { input: 1, ..Default::default() });
        assert_eq!(u.json()["input"], 4);
    }
}
