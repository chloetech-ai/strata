//! One-shot calls to a model through the `claude` CLI in print mode, shared
//! by distillation, consolidation and the query engine.
//!
//! The call runs without tools, from a scratch directory and with Engram
//! disabled, so it records nothing itself. With `isolated` it also skips the
//! user's settings and MCP servers: their plugins, hooks and tool definitions
//! would add tens of thousands of tokens to every call (and another memory
//! plugin would record it).

use std::io::Write;
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

/// The model's reply and what the call cost.
#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub text: String,
    pub cost_usd: f64,
    pub ms: u64,
}

/// Runs the model once on `prompt` with `system` as its system prompt.
pub fn complete(cli: &Cli, system: &str, prompt: &str) -> Result<Reply> {
    let t0 = Instant::now();
    let scratch = std::env::temp_dir().join("engram-distill");
    std::fs::create_dir_all(&scratch)?;
    let mut cmd = Command::new(cli.command);
    cmd.args(["-p", "--system-prompt", system, "--tools", "", "--no-session-persistence", "--output-format", "json"]);
    if cli.isolated {
        cmd.args(["--setting-sources", "project", "--strict-mcp-config"]);
    }
    if !cli.model.is_empty() {
        cmd.args(["--model", cli.model]);
    }
    let mut child = cmd
        .current_dir(&scratch)
        .env("ENGRAM_DISABLE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| err(format!("{}: {e}", cli.command)))?;
    child.stdin.take().unwrap().write_all(prompt.as_bytes())?;
    let deadline = Instant::now() + cli.timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err("model call timed out"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output()?;
    let v: Json = serde_json::from_slice(&out.stdout).map_err(|e| err(format!("model output: {e}")))?;
    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Err(err(format!("model call failed: {}", v.get("result").and_then(|r| r.as_str()).unwrap_or(""))));
    }
    Ok(Reply {
        text: v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string(),
        cost_usd: v.get("total_cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0),
        ms: t0.elapsed().as_millis() as u64,
    })
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
}
