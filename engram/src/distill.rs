//! Knowledge a later session will need, written down by a small model after
//! each turn: decisions and their reasons, rules and conventions,
//! requirements for planned work, and facts about the project and its
//! environment. Much of this is said only in conversation and never reaches
//! the code, which is where a fresh session would look for it.
//!
//! The model is called through the `claude` CLI in print mode, without
//! tools, from a scratch directory and with Engram disabled, so the call
//! records nothing itself. It runs in the daemon after the hook has
//! answered, so it never delays a prompt.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::record::{Draft, Kind};
use crate::redact::redact;
use crate::{err, Result};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DistillConfig {
    pub enabled: bool,
    /// The CLI to call (`claude`).
    pub command: String,
    /// Its `--model` argument; a small, fast model is enough. Empty: the
    /// CLI's default. `ENGRAM_DISTILL_MODEL` overrides it.
    pub model: String,
    pub timeout_secs: u64,
}

impl Default for DistillConfig {
    fn default() -> Self {
        DistillConfig { enabled: true, command: "claude".into(), model: String::new(), timeout_secs: 120 }
    }
}

impl DistillConfig {
    /// Enabled, and not switched off with `ENGRAM_DISTILL=0`.
    pub fn active(&self) -> bool {
        self.enabled && std::env::var("ENGRAM_DISTILL").map_or(true, |v| v != "0")
    }

    /// The model to ask for, if any.
    pub fn model(&self) -> String {
        std::env::var("ENGRAM_DISTILL_MODEL").ok().filter(|m| !m.is_empty()).unwrap_or_else(|| self.model.clone())
    }
}

const SYSTEM: &str = "You keep the long-term notes of a software project. You read one exchange between a developer and a coding assistant and write down what a future session will need. Output JSON lines only.";

const PROMPT: &str = r#"Here is one exchange from a software project.

<request>
{request}
</request>

<commands_run>
{commands}
</commands_run>

<assistant_reply>
{answer}
</assistant_reply>

<already_noted>
{known}
</already_noted>

Write down what someone working on this project in a later session would need to know and could not easily see in the code:
- decisions, with the reason given;
- rules, constraints and conventions the user, the team or a client set: what must or must not be done, formats, naming, processes. Keep every rule stated, with its reason, even if the current code already follows it: the code shows what was done, not that it must stay that way;
- requirements for work the user announced or planned but that is not done yet, with every detail given (formats, names, numbers);
- facts about the project or its environment that were stated or discovered (how to run or test it, quirks, people, external systems), and approaches that were tried and failed, with why.

Each item is one self-contained sentence that names its subject and keeps the exact details and the reason. One item per distinct point; a planned piece of work with all its requirements is one item. With each item list the entities it is about, so that notes about the same thing can be found together: the components, features, files, data formats, external systems and people it concerns (short names, e.g. "count sheet", "CSV export", "items.py", "Petra Keller").

Leave out:
- anything already noted above, unless this exchange changes it;
- status that will soon be stale (test counts, what is committed or pushed, which branch is checked out);
- the assistant's own working conventions (commit message trailers, tool usage) and its suggestions the user did not take up;
- details of the code that reading it shows (signatures, what a function does).

Output one JSON object per line and nothing else:
{"kind": "decision" | "rule" | "requirement" | "fact", "text": "<sentence>", "entities": ["<entity>", ...]}
{"kind": "decision" | "rule" | "requirement" | "fact", "text": "<new sentence>", "entities": ["<entity>", ...], "replaces": "<id of the noted item it updates>"}
{"kind": "done", "id": "<id of a noted requirement that this exchange completed>"}
If there is nothing worth keeping, output nothing."#;

/// What one exchange yields: new knowledge, noted items it replaces, and
/// planned work it completed.
#[derive(Debug, Default)]
pub struct Distilled {
    pub drafts: Vec<Draft>,
    pub replaced: Vec<String>,
    pub done: Vec<String>,
    pub cost_usd: f64,
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut i = n;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &s[..i])
}

/// The label a noted record carries in the prompt.
pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Todo => "requirement",
        Kind::Preference => "rule",
        Kind::Decision => "decision",
        _ => "fact",
    }
}

/// The prompt for one exchange; `known` lists (id, kind, text) of what is
/// already noted.
pub fn prompt_for(request: &str, answer: &str, commands: &[String], known: &[(String, Kind, String)]) -> String {
    let known = if known.is_empty() { "(nothing yet)".to_string() } else { known.iter().map(|(id, k, t)| format!("[{id}] {}: {}", label(*k), clip(t, 300))).collect::<Vec<_>>().join("\n") };
    let commands = if commands.is_empty() { "(none)".to_string() } else { commands.join("\n") };
    PROMPT
        .replace("{request}", &clip(&redact(request), 6000))
        .replace("{commands}", &clip(&redact(&commands), 1500))
        .replace("{answer}", &clip(&redact(answer), 4000))
        .replace("{known}", &known)
}

/// Parses the model's JSON lines; anything else is ignored.
pub fn parse(text: &str) -> Distilled {
    let mut out = Distilled::default();
    let id_of = |v: &Json, key: &str| v.get(key).and_then(|i| i.as_str()).map(|i| i.trim().trim_matches(|c| c == '[' || c == ']').to_string()).filter(|i| !i.is_empty());
    for line in text.lines() {
        let line = line.trim().trim_end_matches(',');
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Json>(line) else { continue };
        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        if kind == "done" {
            out.done.extend(id_of(&v, "id"));
            continue;
        }
        let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("").trim();
        if text.len() < 12 {
            continue;
        }
        let (k, tag) = match kind {
            "decision" => (Kind::Decision, "distilled"),
            "rule" => (Kind::Preference, "distilled"),
            "requirement" => (Kind::Todo, "planned"),
            "fact" => (Kind::Fact, "distilled"),
            _ => continue,
        };
        let replaces = id_of(&v, "replaces");
        let entities: Vec<String> = v.get("entities").and_then(|e| e.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).map(|x| clip(x.trim(), 60)).filter(|x| !x.is_empty()).take(6).collect()).unwrap_or_default();
        let mut d = Draft::new(k, clip(&redact(text), 600)).tag(tag).files(crate::capture::paths_in(text)).entities(entities);
        if let Some(r) = &replaces {
            d = d.link("supersedes", r.clone());
        }
        out.replaced.extend(replaces);
        out.drafts.push(d);
    }
    out
}

/// Runs the model on one exchange.
pub fn distill(cfg: &DistillConfig, request: &str, answer: &str, commands: &[String], known: &[(String, Kind, String)]) -> Result<Distilled> {
    let prompt = prompt_for(request, answer, commands, known);
    let scratch = std::env::temp_dir().join("engram-distill");
    std::fs::create_dir_all(&scratch)?;
    let mut cmd = Command::new(&cfg.command);
    cmd.args(["-p", "--system-prompt", SYSTEM, "--tools", "", "--no-session-persistence", "--output-format", "json"]);
    let model = cfg.model();
    if !model.is_empty() {
        cmd.args(["--model", &model]);
    }
    let mut child = cmd
        .current_dir(&scratch)
        .env("ENGRAM_DISABLE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| err(format!("{}: {e}", cfg.command)))?;
    child.stdin.take().unwrap().write_all(prompt.as_bytes())?;
    let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err("distillation timed out"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output()?;
    let v: Json = serde_json::from_slice(&out.stdout).map_err(|e| err(format!("distillation output: {e}")))?;
    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Err(err(format!("distillation failed: {}", v.get("result").and_then(|r| r.as_str()).unwrap_or(""))));
    }
    let mut d = parse(v.get("result").and_then(|r| r.as_str()).unwrap_or(""));
    d.cost_usd = v.get("total_cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0);
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_items_and_ignores_the_rest() {
        let text = "Here you go:\n{\"kind\": \"decision\", \"text\": \"Storage stays JSON because admins edit the files by hand.\", \"entities\": [\"storage\", \"JSON files\"]}\n\
                    {\"kind\": \"requirement\", \"text\": \"The CSV export uses semicolons and DD.MM.YYYY dates.\", \"replaces\": \"abc:3\"},\n\
                    {\"kind\": \"done\", \"id\": \"[abc:12]\"}\n{\"kind\": \"mood\", \"text\": \"The user seems happy today.\"}\nnot json";
        let d = parse(text);
        assert_eq!(d.drafts.len(), 2);
        assert_eq!(d.drafts[0].kind, Kind::Decision);
        assert_eq!(d.drafts[0].entities, vec!["storage".to_string(), "JSON files".to_string()]);
        assert_eq!(d.drafts[1].links, vec![crate::record::Link::new("supersedes", "abc:3")]);
        assert_eq!(d.drafts[1].kind, Kind::Todo);
        assert_eq!(d.drafts[1].tag, "planned");
        assert_eq!(d.replaced, vec!["abc:3".to_string()]);
        assert_eq!(d.done, vec!["abc:12".to_string()]);
    }

    #[test]
    fn prompt_carries_the_exchange() {
        let p = prompt_for("Add X. The client wants Y.", "Added X.", &["make test".into()], &[("s:1".into(), Kind::Todo, "Build Z".into())]);
        assert!(p.contains("The client wants Y.") && p.contains("[s:1] requirement: Build Z") && p.contains("make test"));
    }
}
