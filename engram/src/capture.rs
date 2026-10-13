//! Turns hook payloads into record drafts: chunking, per-tool shaping,
//! preference cues, compaction-summary sections and episode cards.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value as Json;

use crate::record::{Draft, Kind};
use crate::redact::redact;

/// Largest chunk stored as one record. Small records make precise keys and
/// fill a context budget with more distinct evidence (results/structure).
pub const CHUNK_CHARS: usize = 400;
/// The unit long outputs are kept or dropped in (see [`head_tail_chunks`]).
pub const SOURCE_CHARS: usize = 1500;
/// Results longer than this many source units keep only their head and tail.
pub const KEEP_HEAD_CHUNKS: usize = 3;

fn char_floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Splits `text` into chunks of at most `max` bytes, preferring line, then
/// sentence, then word boundaries.
pub fn chunk(text: &str, max: usize) -> Vec<String> {
    chunk_spans(text, max).into_iter().map(|(s, e)| text[s..e].to_string()).collect()
}

/// The byte ranges of [`chunk`]'s pieces in `text`.
pub fn chunk_spans(text: &str, max: usize) -> Vec<(usize, usize)> {
    let lead = text.len() - text.trim_start().len();
    let trimmed = text.trim();
    let mut out = Vec::new();
    if trimmed.is_empty() {
        return out;
    }
    // `rest` is text[base..base + rest.len()].
    let mut base = lead;
    let mut rest = trimmed;
    let mut push = |s: usize, piece: &str| {
        let t = piece.trim();
        if !t.is_empty() {
            let s = s + (piece.len() - piece.trim_start().len());
            out.push((s, s + t.len()));
        }
    };
    while rest.len() > max {
        let cut = char_floor(rest, max);
        let window = &rest[..cut];
        let at = window
            .rfind("\n\n")
            .filter(|&i| i > max / 2)
            .or_else(|| window.rfind('\n').filter(|&i| i > max / 2))
            .or_else(|| window.rfind(". ").map(|i| i + 1).filter(|&i| i > max / 2))
            .or_else(|| window.rfind(' ').filter(|&i| i > max / 3))
            .unwrap_or(cut);
        let at = if at == 0 { cut.max(1) } else { at };
        let at = char_floor(rest, at).max(rest.char_indices().nth(1).map_or(rest.len(), |(i, _)| i));
        push(base, &rest[..at]);
        let next = rest[at..].trim_start();
        base += rest.len() - next.len();
        rest = next;
    }
    if !rest.is_empty() {
        push(base, rest);
    }
    out
}

/// Chunks of a long output: the first [`KEEP_HEAD_CHUNKS`] source units and
/// the last, with a marker for what was dropped, cut into records.
pub fn head_tail_chunks(text: &str) -> Vec<String> {
    let mut c = chunk(text, SOURCE_CHARS);
    if c.len() > KEEP_HEAD_CHUNKS + 1 {
        let dropped: usize = c[KEEP_HEAD_CHUNKS..c.len() - 1].iter().map(|x| x.len()).sum();
        let last = c.pop().unwrap();
        c.truncate(KEEP_HEAD_CHUNKS);
        c.push(format!("[… {dropped} characters omitted …]\n{last}"));
    }
    c.iter().flat_map(|u| chunk(u, CHUNK_CHARS)).collect()
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!("{}…", &s[..char_floor(s, max)])
}

/// File paths mentioned in `text` (source-like extensions), at most 8.
pub fn paths_in(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:[A-Za-z0-9_.\-]+/)*[A-Za-z0-9_\-]+\.(?:rs|py|ts|tsx|js|jsx|go|java|kt|c|cc|cpp|h|hpp|cs|rb|php|swift|scala|sh|md|json|toml|yaml|yml|sql|html|css|txt|cfg|ini|lock)\b").unwrap()
    });
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        let p = m.as_str().trim_start_matches("./").to_string();
        if seen.insert(p.clone()) {
            out.push(p);
            if out.len() == 8 {
                break;
            }
        }
    }
    out
}

/// Path made relative to the project when inside it.
fn rel(path: &str, project: &str) -> String {
    let p = path.strip_prefix(project).map(|p| p.trim_start_matches('/')).unwrap_or(path);
    p.to_string()
}

// ---------------------------------------------------------------- prompts

/// Sentences of a user prompt that state a lasting preference or rule.
pub fn preference_sentences(prompt: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)\b(always|never|don'?t ever|do not ever|from now on|going forward|make sure (?:to|that|you)|please (?:always|never|don'?t|do not)|i (?:prefer|want you to|don'?t want)|remember (?:that|to)|keep in mind|by default|should (?:always|never)|must (?:always|never|not))\b").unwrap()
    });
    let mut out = Vec::new();
    for s in prompt.split(|c| c == '\n' || c == '.' || c == '!' || c == '?').map(str::trim) {
        if s.len() >= 12 && s.len() <= 400 && re.is_match(s) {
            out.push(s.to_string());
        }
    }
    out
}

/// A user prompt: the prompt itself plus any preference sentences.
pub fn prompt_drafts(prompt: &str) -> Vec<Draft> {
    let prompt = redact(prompt);
    let files = paths_in(&prompt);
    let mut out: Vec<Draft> = chunk(&prompt, CHUNK_CHARS).into_iter().map(|c| Draft::new(Kind::Prompt, c).files(files.clone())).collect();
    for s in preference_sentences(&prompt) {
        out.push(Draft::new(Kind::Preference, format!("User preference: {s}")).tag("cue").files(paths_in(&s)));
    }
    out
}

/// Whether a prompt refers back to earlier work.
pub fn past_reference(prompt: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)\b(earlier|previous(?:ly)?|before|last (?:time|session|week|night)|yesterday|we (?:decided|agreed|discussed|said|chose|used|had|did|tried|found)|you (?:said|suggested|mentioned|did|wrote|found)|again|remember|recall|as (?:discussed|before|planned)|that (?:bug|issue|error|approach|idea|fix|number|result)|the same|originally|did we|have we|were we|go back)\b").unwrap()
    });
    re.is_match(prompt)
}

// ---------------------------------------------------------------- tools

/// Text of a tool response, whatever its shape.
pub fn response_text(resp: &Json) -> String {
    match resp {
        Json::Null => String::new(),
        Json::String(s) => s.clone(),
        Json::Array(a) => a.iter().map(response_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n"),
        Json::Object(o) => {
            let mut parts = Vec::new();
            for k in ["stdout", "stderr", "output", "content", "result", "text", "message", "error", "results", "file"] {
                if let Some(v) = o.get(k) {
                    let t = response_text(v);
                    if !t.is_empty() {
                        parts.push(if k == "stderr" { format!("[stderr]\n{t}") } else { t });
                    }
                }
            }
            if parts.is_empty() {
                serde_json::to_string(resp).unwrap_or_default()
            } else {
                parts.join("\n")
            }
        }
        other => other.to_string(),
    }
}

/// Whether a tool result reports a failure.
pub fn looks_like_error(resp: &Json, text: &str) -> bool {
    if resp.get("is_error").and_then(|v| v.as_bool()) == Some(true) || resp.get("interrupted").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    if let Some(code) = resp.get("exit_code").or_else(|| resp.get("returnCode")).and_then(|v| v.as_i64()) {
        if code != 0 {
            return true;
        }
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?m)(^error(\[E\d+\])?:|\bpanicked at\b|^Traceback \(most recent call last\)|\bFAILED\b|^fatal:|\bSegmentation fault\b|\bcommand not found\b|\bNo such file or directory\b|test result: FAILED)").unwrap());
    re.is_match(text)
}

/// The drafts of one tool call and its result.
pub fn tool_drafts(tool: &str, input: &Json, resp: &Json, project: &str) -> Vec<Draft> {
    if tool.starts_with("mcp__engram__") {
        return Vec::new();
    }
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let path = {
        let p = s("file_path");
        let p = if p.is_empty() { s("notebook_path") } else { p };
        let p = if p.is_empty() { s("path") } else { p };
        rel(&p, project)
    };
    let files = if path.is_empty() { Vec::new() } else { vec![path.clone()] };
    let out_text = redact(&response_text(resp));
    let failed = looks_like_error(resp, &out_text);
    let mut d = Vec::new();
    let result_chunks = |kind: Kind, files: Vec<String>, d: &mut Vec<Draft>| {
        for c in head_tail_chunks(&out_text) {
            let mut f = files.clone();
            f.extend(paths_in(&c).into_iter().take(4));
            f.dedup();
            d.push(Draft::new(kind, c).tag(tool).files(f));
        }
    };
    match tool {
        "Bash" => {
            let cmd = redact(&s("command"));
            let desc = s("description");
            let call = if desc.is_empty() { format!("$ {}", clip(&cmd, 1200)) } else { format!("$ {}  # {}", clip(&cmd, 1100), clip(&desc, 200)) };
            d.push(Draft::new(Kind::ToolCall, call).tag(tool).files(paths_in(&cmd)));
            result_chunks(if failed { Kind::Error } else { Kind::ToolResult }, paths_in(&cmd), &mut d);
        }
        "Read" => {
            let lines = out_text.lines().count();
            d.push(Draft::new(Kind::ToolCall, format!("Read {path} ({lines} lines)")).tag(tool).files(files));
        }
        "Edit" | "MultiEdit" => {
            let edits: Vec<(String, String)> = match input.get("edits").and_then(|e| e.as_array()) {
                Some(list) => list
                    .iter()
                    .map(|e| (e.get("old_string").and_then(|v| v.as_str()).unwrap_or("").to_string(), e.get("new_string").and_then(|v| v.as_str()).unwrap_or("").to_string()))
                    .collect(),
                None => vec![(s("old_string"), s("new_string"))],
            };
            for (old, new) in edits {
                let text = redact(&format!("Edit {path}: replaced\n{}\nwith\n{}", clip(&old, 300), clip(&new, 1100)));
                d.push(Draft::new(Kind::ToolCall, text).tag(tool).files(files.clone()));
            }
            if failed {
                for c in chunk(&clip(&out_text, SOURCE_CHARS), CHUNK_CHARS) {
                    d.push(Draft::new(Kind::Error, c).tag(tool).files(files.clone()));
                }
            }
        }
        "Write" => {
            let content = s("content");
            let text = redact(&format!("Write {path} ({} characters):\n{}", content.len(), clip(&content, 1200)));
            d.push(Draft::new(Kind::ToolCall, text).tag(tool).files(files));
        }
        "Grep" | "Glob" | "LS" => {
            let pat = s("pattern");
            d.push(Draft::new(Kind::ToolCall, format!("{tool} {pat} {path}").trim().to_string()).tag(tool).files(files.clone()));
            if !out_text.trim().is_empty() {
                d.push(Draft::new(Kind::ToolResult, clip(&out_text, 800)).tag(tool).files(paths_in(&out_text)));
            }
        }
        "WebFetch" | "WebSearch" => {
            let what = if tool == "WebFetch" { s("url") } else { s("query") };
            d.push(Draft::new(Kind::ToolCall, format!("{tool} {what}")).tag(tool));
            for c in head_tail_chunks(&out_text) {
                d.push(Draft::new(Kind::ToolResult, c).tag(tool).importance(0.4));
            }
        }
        "Task" | "Agent" => {
            let desc = s("description");
            let prompt = redact(&s("prompt"));
            d.push(Draft::new(Kind::ToolCall, format!("Subagent: {desc}\n{}", clip(&prompt, 1200))).tag(tool));
            for c in head_tail_chunks(&out_text) {
                d.push(Draft::new(Kind::ToolResult, c).tag(tool).importance(0.5));
            }
        }
        "TodoWrite" => {
            let todos = input.get("todos").and_then(|t| t.as_array()).cloned().unwrap_or_default();
            let lines: Vec<String> = todos
                .iter()
                .map(|t| format!("[{}] {}", t.get("status").and_then(|v| v.as_str()).unwrap_or("?"), t.get("content").and_then(|v| v.as_str()).unwrap_or("")))
                .collect();
            if !lines.is_empty() {
                d.push(Draft::new(Kind::Todo, format!("Todo list:\n{}", lines.join("\n"))).tag(tool));
            }
        }
        _ => {
            let args = redact(&clip(&serde_json::to_string(input).unwrap_or_default(), 600));
            d.push(Draft::new(Kind::ToolCall, format!("{tool} {args}")).tag(tool).files(files.clone()));
            result_chunks(if failed { Kind::Error } else { Kind::ToolResult }, files, &mut d);
        }
    }
    d
}

// ---------------------------------------------------------------- answers and summaries

/// Assistant text between tool calls: what the model concluded along the way.
pub fn narration_drafts(text: &str) -> Vec<Draft> {
    let text = redact(text);
    if text.len() < 40 {
        return Vec::new();
    }
    let files = paths_in(&text);
    chunk(&text, CHUNK_CHARS).into_iter().map(|c| Draft::new(Kind::Answer, c).tag("narration").files(files.clone()).importance(0.4)).collect()
}

/// Whether an answer says the model did not know or could not find out.
/// Such answers are kept (low importance, tag `uncertain`) but never injected:
/// recalling them would only repeat the failure.
pub fn is_uncertain(answer: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)^\W*(i don'?t know|i do not know|i couldn'?t (?:find|answer|tell|determine)|i can'?t (?:find|answer|tell|determine|say)|i'?m not sure|i have no (?:record|information|way)|i couldn'?t answer|nothing in (?:this|the) (?:conversation|context))").unwrap()
    });
    re.is_match(answer.trim_start())
}

pub fn answer_drafts(text: &str) -> Vec<Draft> {
    let text = redact(text);
    let files = paths_in(&text);
    let uncertain = is_uncertain(&text);
    chunk(&text, CHUNK_CHARS)
        .into_iter()
        .map(|c| {
            let d = Draft::new(Kind::Answer, c).files(files.clone());
            if uncertain {
                d.tag("uncertain").importance(0.1)
            } else {
                d
            }
        })
        .collect()
}

/// A compaction summary, one or more records per numbered section
/// ("1. Primary Request and Intent:", "7. Pending Tasks:", ...).
pub fn summary_drafts(summary: &str) -> Vec<Draft> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?m)^\s*(\d{1,2})\.\s+([A-Z][A-Za-z ,&/'()-]{2,60}):").unwrap());
    let summary = redact(summary);
    let heads: Vec<(usize, String)> = re.captures_iter(&summary).map(|c| (c.get(0).unwrap().start(), c[2].trim().to_string())).collect();
    let mut sections: Vec<(String, &str)> = Vec::new();
    if heads.len() < 2 {
        sections.push(("summary".into(), summary.as_str()));
    } else {
        for (i, (start, name)) in heads.iter().enumerate() {
            let end = heads.get(i + 1).map_or(summary.len(), |h| h.0);
            sections.push((name.clone(), &summary[*start..end]));
        }
    }
    let mut out = Vec::new();
    for (name, body) in sections {
        let tag = name.to_lowercase().replace(' ', "_");
        let kind = if tag.contains("pending") || tag.contains("next_step") { Kind::Todo } else { Kind::Summary };
        for c in chunk(body, CHUNK_CHARS) {
            out.push(Draft::new(kind, c.clone()).tag(tag.clone()).files(paths_in(&c)).importance(0.8));
        }
    }
    out
}

/// What one user turn did, accumulated from its hooks.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct EpisodeAcc {
    pub prompt: String,
    pub files: BTreeSet<String>,
    pub commands: Vec<String>,
    pub errors: Vec<String>,
    pub tools: usize,
}

impl EpisodeAcc {
    pub fn add_tool(&mut self, tool: &str, input: &Json, resp: &Json, project: &str) {
        self.tools += 1;
        for k in ["file_path", "notebook_path"] {
            if let Some(p) = input.get(k).and_then(|v| v.as_str()) {
                if matches!(tool, "Edit" | "MultiEdit" | "Write" | "NotebookEdit") {
                    self.files.insert(rel(p, project));
                }
            }
        }
        if tool == "Bash" {
            if let Some(c) = input.get("command").and_then(|v| v.as_str()) {
                if self.commands.len() < 6 {
                    self.commands.push(clip(&redact(c.lines().next().unwrap_or("")), 120));
                }
            }
            let text = response_text(resp);
            if looks_like_error(resp, &text) && self.errors.len() < 3 {
                let line = text.lines().find(|l| looks_like_error(&Json::Null, l)).unwrap_or_else(|| text.lines().next().unwrap_or(""));
                self.errors.push(clip(&redact(line.trim()), 200));
            }
        }
    }

    /// The episode card, given the turn's final answer.
    pub fn card(&self, turn: i64, answer: &str) -> Option<Draft> {
        if self.prompt.is_empty() && answer.is_empty() {
            return None;
        }
        let mut t = format!("Turn {turn}. User asked: {}", clip(&self.prompt, 400));
        if !self.files.is_empty() {
            t += &format!("\nChanged: {}", self.files.iter().take(10).cloned().collect::<Vec<_>>().join(", "));
        }
        if !self.commands.is_empty() {
            t += &format!("\nRan: {}", self.commands.join(" ; "));
        }
        if !self.errors.is_empty() {
            t += &format!("\nErrors seen: {}", self.errors.join(" | "));
        }
        if !answer.is_empty() {
            t += &format!("\nOutcome: {}", clip(&redact(answer), 600));
        }
        let d = Draft::new(Kind::Episode, t).files(self.files.iter().cloned().collect());
        Some(if is_uncertain(answer) { d.tag("uncertain").importance(0.1) } else { d })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chunks_respect_limits_and_boundaries() {
        let text = (0..200).map(|i| format!("line {i} with some words in it")).collect::<Vec<_>>().join("\n");
        let c = chunk(&text, 300);
        assert!(c.iter().all(|x| x.len() <= 300));
        assert!(c.iter().all(|x| x.starts_with("line ")));
        assert_eq!(c.join("\n"), text);
        let padded = format!("  \n{text}\n\n");
        let spans = chunk_spans(&padded, 300);
        assert_eq!(spans.iter().map(|&(s, e)| &padded[s..e]).collect::<Vec<_>>(), c);
        assert!(spans.windows(2).all(|w| w[0].1 < w[1].0));
        // 200 lines of ~30 bytes: 5 source units, of which the first three
        // and the last are kept, each cut into records.
        let ht = head_tail_chunks(&text);
        let units = chunk(&text, SOURCE_CHARS);
        assert!(units.len() > KEEP_HEAD_CHUNKS + 1);
        assert!(ht.iter().all(|x| x.len() <= CHUNK_CHARS));
        assert!(ht.join("\n").starts_with(&units[..KEEP_HEAD_CHUNKS].join("\n")));
        assert_eq!(ht.iter().filter(|x| x.contains("omitted")).count(), 1);
        assert!(ht.join("\n").ends_with(units.last().unwrap().as_str()));
        assert_eq!(chunk("é".repeat(1000).as_str(), 301).concat(), "é".repeat(1000));
    }

    #[test]
    fn preferences_and_past_references() {
        let p = preference_sentences("Fix the test. From now on, always run clippy before committing! Thanks");
        assert_eq!(p, vec!["From now on, always run clippy before committing"]);
        assert!(preference_sentences("Implement the parser").is_empty());
        assert!(past_reference("What did we decide about the planner earlier?"));
        assert!(!past_reference("Add a unit test for chunk()"));
    }

    #[test]
    fn tools_are_shaped() {
        let d = tool_drafts("Bash", &json!({"command": "cargo test", "description": "Run tests"}), &json!({"stdout": "error[E0599]: no method named `foo`", "stderr": ""}), "/p");
        assert_eq!(d[0].kind, Kind::ToolCall);
        assert_eq!(d[1].kind, Kind::Error);
        let d = tool_drafts("Read", &json!({"file_path": "/p/src/lib.rs"}), &json!({"file": {"content": "a\nb"}}), "/p");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].files, vec!["src/lib.rs"]);
        let d = tool_drafts("Edit", &json!({"file_path": "/p/src/a.rs", "old_string": "x", "new_string": "y"}), &json!({}), "/p");
        assert!(d[0].text.starts_with("Edit src/a.rs"));
        assert!(tool_drafts("mcp__engram__memory_search", &json!({}), &json!({}), "/p").is_empty());
    }

    #[test]
    fn uncertain_answers() {
        assert!(is_uncertain("I don't know. Nothing in this conversation mentions it."));
        assert!(is_uncertain("**I couldn't find** that"));
        assert!(!is_uncertain("The test was dense_runs_are_nearly_free. I don't know why it failed."));
        assert_eq!(answer_drafts("I do not know.")[0].tag, "uncertain");
    }

    #[test]
    fn summary_sections() {
        let s = "Summary:\n1. Primary Request and Intent:\n   build a thing\n2. Key Technical Concepts:\n   - BM25\n7. Pending Tasks:\n   - write report\n";
        let d = summary_drafts(s);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].tag, "primary_request_and_intent");
        assert_eq!(d[2].kind, Kind::Todo);
    }
}
