//! Turns hook payloads into record drafts: chunking, per-tool shaping,
//! preference cues, compaction-summary sections and episode cards.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value as Json;

use crate::record::{Draft, Kind, Source};
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
    for s in sentences(prompt) {
        if s.len() >= 12 && s.len() <= 400 && re.is_match(s) {
            out.push(s.to_string());
        }
    }
    out
}

/// `text` split into sentences: at line breaks, `!`, `?`, and a `.` that
/// ends a word (not the one in "3.8" or "config.py").
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let next = it.peek().map(|x| x.1);
        let end = match c {
            '\n' | '!' | '?' => true,
            '.' => next.map_or(true, char::is_whitespace),
            _ => false,
        };
        if end {
            out.push(text[start..i].trim());
            start = i + c.len_utf8();
        }
    }
    out.push(text[start..].trim());
    out.retain(|s| !s.is_empty());
    out
}

/// A user prompt: the prompt itself plus any preference sentences. A
/// harness's envelopes around it ([`crate::envelope`]) are left out, so
/// that a reminder's "never ..." is not taken for the user's rule.
pub fn prompt_drafts(prompt: &str) -> Vec<Draft> {
    prompt_drafts_by(prompt, &Source::User)
}

/// A prompt written by `by`. Only a person's prompt yields preference
/// cues: another agent's brief ("by default, shipping is 499") is that
/// agent's instruction for one task, never the user's preference. A
/// prompt someone other than the user wrote is stored with its source.
pub fn prompt_drafts_by(prompt: &str, by: &Source) -> Vec<Draft> {
    let prompt = redact(&crate::envelope::clean(prompt));
    if prompt.trim().is_empty() {
        return Vec::new();
    }
    let files = paths_in(&prompt);
    let mark = |d: Draft| if by.is_user() { d } else { d.by(by) };
    let mut out: Vec<Draft> = chunk(&prompt, CHUNK_CHARS).into_iter().map(|c| mark(Draft::new(Kind::Prompt, c).files(files.clone()))).collect();
    if by.is_user() {
        for s in preference_sentences(&prompt) {
            out.push(Draft::new(Kind::Preference, format!("User preference: {s}")).tag("cue").files(paths_in(&s)));
        }
        for s in decision_sentences(&prompt) {
            out.push(Draft::new(Kind::Decision, format!("The user decided: {s}")).tag("stated").files(paths_in(&s)).by(by));
        }
    }
    out
}

/// Words that make an instruction a standing rule rather than a one-off
/// for the task at hand: "always", "never", "from now on", "by default",
/// "every time", "as a rule", "in future", "our convention"...
fn standing_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(always|never|from now on|from here on|going forward|in (?:the )?future|for future|by default|every time|each time|whenever|any ?time|as a (?:general )?rule|in general|(?:our|the team'?s|the project'?s|house) (?:convention|rule|policy|standard)s?|convention|policy|standing rule|for (?:all|every) (?:future )?(?:tasks?|work|changes|sessions|agents|prs?))\b").unwrap()
    })
}

/// Whether a rule the distiller found in an exchange is a standing one:
/// the user said so in words ("always", "never", "from now on" ... in a
/// sentence of the request that shares its subject), or answered a
/// question of the assistant that asked for a rule (its previous message
/// asked whether to do something always, from now on, by default...).
/// Anything else the user told an agent to do or not do is an instruction
/// for that task ("Don't touch money.py or cart.py").
pub fn standing_rule(request: &str, prev_answer: &str, rule: &str) -> bool {
    let re = standing_re();
    let terms = |t: &str| -> BTreeSet<String> { chloedb::text::analyze(t).into_iter().collect() };
    let rule_terms = terms(rule);
    for s in sentences(request) {
        if !re.is_match(s) {
            continue;
        }
        let st = terms(s);
        let shared = st.intersection(&rule_terms).count();
        if shared >= 2.min(st.len().max(1)) || rule_terms.is_empty() {
            return true;
        }
    }
    // An answer to a question asking for a rule: the request is short (a
    // "yes", a choice) and the question before it asked about the future.
    let short = request.split_whitespace().count() <= 40;
    short && sentences(prev_answer).into_iter().rev().take(6).any(|s| re.is_match(s) && prev_answer.contains(&format!("{s}?")))
}

/// The tag of a note copied from a memory file an agent wrote.
pub const MEMORY_FILE_TAG: &str = "memory-file";

/// A path outside the project (absolute) in a memory folder: Claude Code's
/// own (`~/.claude/projects/<p>/memory/`, `/tmp/claude/memory/...`).
pub fn is_memory_file(path: &str) -> bool {
    path.starts_with('/') && path.ends_with(".md") && path.split('/').any(|c| c == "memory")
}

/// Sentences of a person's prompt that state a decision ("My decision:
/// customers win", "We decided to ..."): kept as the user's decisions at
/// once, whether or not the distiller runs, so a decision made in
/// Workbench (a steer, a typed answer) is always a ChloeCtx record.
pub fn decision_sentences(prompt: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)^\W*(?:my decision|decision|our decision|the decision is|i(?:'ve| have)? decided|we(?:'ve| have)? decided|i decide|ruling)\b").unwrap());
    // Sentences split at line ends, so a decision's "X: y. Because z." keeps its line.
    let mut out = Vec::new();
    for line in prompt.lines() {
        let l = line.trim();
        if re.is_match(l) && l.len() >= 16 {
            out.push(clip(l, 600));
        }
    }
    if out.is_empty() {
        for s in sentences(prompt) {
            if re.is_match(s) && s.len() >= 16 {
                out.push(clip(s, 600));
            }
        }
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

/// ChloeCtx's own tools, whose calls and answers are not recorded: the
/// memory tools (named `mcp__engram__*` before the rename, and
/// `mcp__plugin_chloectx_chloectx__*` when the plugin serves them), and a mounted
/// capsule's, under whatever name its MCP server was given (`capsule` in
/// the docs). A capsule's answers are another project's memory, possibly
/// from someone else: recorded here they would outlive the mount, be
/// recalled as this project's own history, be pushed by sync and be packed
/// into the next capsule, where a forget in their own project cannot reach.
pub fn is_own_tool(tool: &str) -> bool {
    if tool.starts_with("mcp__chloectx__") || tool.starts_with("mcp__engram__") || tool.starts_with(crate::plugin::TOOL_PREFIX) {
        return true;
    }
    tool.strip_prefix("mcp__").and_then(|t| t.rsplit_once("__")).is_some_and(|(_, name)| matches!(name, "capsule_ask" | "capsule_search" | "capsule_timeline" | "capsule_sql"))
}

/// The drafts of one tool call and its result.
pub fn tool_drafts(tool: &str, input: &Json, resp: &Json, project: &str) -> Vec<Draft> {
    if is_own_tool(tool) {
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
    // A note the agent kept in a memory file of its own, outside the
    // repository (Claude Code's memory folder): what it says is kept here
    // too, saying where the file is, so that it is not only somewhere
    // Workbench and later sessions never look.
    if matches!(tool, "Write" | "Edit" | "MultiEdit") && !failed && is_memory_file(&path) {
        let body = if tool == "Write" { s("content") } else { input.get("edits").and_then(|e| e.as_array()).map(|l| l.iter().filter_map(|e| e.get("new_string").and_then(|v| v.as_str())).collect::<Vec<_>>().join("\n")).unwrap_or_else(|| s("new_string")) };
        // Without its front matter (`---` ... `---`).
        let body = match body.trim_start().strip_prefix("---") {
            Some(rest) => rest.split_once("\n---").map(|(_, b)| b.to_string()).unwrap_or_default(),
            None => body,
        };
        let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
        if body.len() >= 20 {
            d.push(Draft::new(Kind::Note, format!("The agent kept this in its own memory file {path} (outside the repository): {}", clip(&redact(&body), 600))).tag(MEMORY_FILE_TAG).ctx(format!("Lives: {path}")));
        }
    }
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
            // Redacted before they are cut, so that no part of a secret the
            // cut leaves too short to recognise is kept.
            for (old, new) in edits {
                let text = format!("Edit {path}: replaced\n{}\nwith\n{}", clip(&redact(&old), 300), clip(&redact(&new), 1100));
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
            let text = format!("Write {path} ({} characters):\n{}", content.len(), clip(&redact(&content), 1200));
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
            // The input's strings, and values under keys that name a
            // secret, redacted before it is cut.
            let mut input = input.clone();
            crate::redact::redact_json(&mut input);
            let args = clip(&serde_json::to_string(&input).unwrap_or_default(), 600);
            d.push(Draft::new(Kind::ToolCall, format!("{tool} {args}")).tag(tool).files(files.clone()));
            result_chunks(if failed { Kind::Error } else { Kind::ToolResult }, files, &mut d);
        }
    }
    d
}

/// The line a tool call's record carries in its context when the call took
/// a second or more: how long, in whole seconds, from the call to its
/// result (`chloectx report` reads it; the record's `ts` is the result's).
pub const TOOK: &str = "Took: ";

/// Notes on the first tool call of `drafts` that the call took `secs`
/// seconds (nothing under a second).
pub fn took(drafts: &mut [Draft], secs: i64) {
    if secs < 1 {
        return;
    }
    if let Some(d) = drafts.iter_mut().find(|d| d.kind == Kind::ToolCall) {
        d.ctx = if d.ctx.is_empty() { format!("{TOOK}{secs}s") } else { format!("{}\n{TOOK}{secs}s", d.ctx) };
    }
}

/// The seconds a tool call's record says it took ([`took`]); 0 when it says
/// nothing.
pub fn took_of(ctx: &str) -> i64 {
    ctx.lines().find_map(|l| l.strip_prefix(TOOK)).and_then(|s| s.trim_end_matches('s').parse().ok()).unwrap_or(0)
}

/// The tag of an event record of a sub-agent that finished.
pub const SUBAGENT_STOP: &str = "subagent_stop";
/// The tag of an event record of a notification: `notification:<type>`
/// (`permission_prompt`, `idle_prompt`, ...), or `notification` alone.
pub const NOTIFICATION: &str = "notification";

/// The event record of a `Notification`, `SubagentStop` or `PreCompact`
/// hook, which recall never shows ([`Kind::Event`]). A notification's type comes from
/// the payload, or for an older Claude Code that sends none from its
/// message ("Claude needs your permission to use Bash" is a permission
/// prompt, "Claude is waiting for your input" an idle one).
pub fn event_draft(event: &str, p: &Json) -> Option<Draft> {
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    match event {
        "Notification" => {
            let message = clip(&redact(&s("message")), 300);
            let mut ty = s("notification_type");
            if ty.is_empty() {
                let m = message.to_lowercase();
                ty = if m.contains("permission") {
                    "permission_prompt".into()
                } else if m.contains("waiting for your input") || m.contains("idle") {
                    "idle_prompt".into()
                } else {
                    String::new()
                };
            }
            let ty: String = ty.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect();
            let tag = if ty.is_empty() { NOTIFICATION.to_string() } else { format!("{NOTIFICATION}:{ty}") };
            let text = if message.is_empty() { format!("Notification ({})", if ty.is_empty() { "no message" } else { &ty }) } else { format!("Notification: {message}") };
            Some(Draft::new(Kind::Event, text).tag(tag))
        }
        "SubagentStop" => {
            let ty = clip(&s("agent_type"), 80);
            let id = clip(&s("agent_id"), 80);
            let mut text = if ty.is_empty() { "A sub-agent finished".to_string() } else { format!("Sub-agent {ty} finished") };
            if !id.is_empty() {
                text += &format!(" (agent {id})");
            }
            Some(Draft::new(Kind::Event, text).tag(SUBAGENT_STOP))
        }
        // When a compaction started, which its summary (`PostCompact`) ends.
        "PreCompact" => {
            let trigger: String = s("trigger").chars().filter(|c| c.is_ascii_alphanumeric()).take(20).collect();
            let text = if trigger.is_empty() { "Compaction started".to_string() } else { format!("Compaction started ({trigger})") };
            Some(Draft::new(Kind::Event, text).tag(crate::report::COMPACT_START))
        }
        _ => None,
    }
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
        // Only a person's prompt yields cues; an agent's brief is stored
        // as that agent's, with none.
        let brief = prompt_drafts_by("Add tests. Shipping is 499 by default, never charge it twice.", &Source::Agent("lead".into()));
        assert!(brief.iter().all(|d| d.kind == Kind::Prompt && d.by.as_deref() == Some("agent:lead")), "{brief:?}");
        assert!(prompt_drafts("Shipping is 499 by default, never charge it twice.").iter().any(|d| d.kind == Kind::Preference));
        let p = preference_sentences("Fix the test. From now on, always run clippy before committing! Thanks");
        assert_eq!(p, vec!["From now on, always run clippy before committing"]);
        assert!(preference_sentences("Implement the parser").is_empty());
        assert_eq!(preference_sentences("The PCs run Python 3.8, so never use tomllib in config.py. Thanks"), vec!["The PCs run Python 3.8, so never use tomllib in config.py"]);
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
        assert!(tool_drafts("mcp__chloectx__memory_search", &json!({}), &json!({}), "/p").is_empty());
        assert!(tool_drafts("mcp__engram__memory_search", &json!({}), &json!({}), "/p").is_empty());
        assert!(tool_drafts("mcp__plugin_chloectx_chloectx__memory_ask", &json!({}), &json!({}), "/p").is_empty());
    }

    #[test]
    fn a_mounted_capsules_answers_are_not_recorded() {
        // N-F5: under the name the docs give its server, and any other.
        let answer = json!({"content": [{"type": "text", "text": "Decision: deploy on Fridays. (capsule \"x\" sha256:abc)"}]});
        for tool in ["mcp__capsule__capsule_ask", "mcp__capsule__capsule_search", "mcp__handoff__capsule_sql", "mcp__plugin_x_capsule__capsule_timeline"] {
            assert!(tool_drafts(tool, &json!({"question": "deploys"}), &answer, "/p").is_empty(), "{tool}");
        }
        // Another server's tools still are.
        assert!(!tool_drafts("mcp__github__get_issue", &json!({"n": 1}), &answer, "/p").is_empty());
        assert!(!tool_drafts("mcp__capsule__other", &json!({}), &answer, "/p").is_empty());
    }

    #[test]
    fn events_and_tool_durations() {
        let d = event_draft("Notification", &json!({"message": "Claude needs your permission to use Bash", "notification_type": "permission_prompt"})).unwrap();
        assert_eq!((d.kind, d.tag.as_str(), d.text.as_str()), (Kind::Event, "notification:permission_prompt", "Notification: Claude needs your permission to use Bash"));
        // An older Claude Code names no type: told from the message.
        assert_eq!(event_draft("Notification", &json!({"message": "Claude is waiting for your input"})).unwrap().tag, "notification:idle_prompt");
        assert_eq!(event_draft("Notification", &json!({})).unwrap().tag, "notification");
        let d = event_draft("SubagentStop", &json!({"agent_type": "Explore", "agent_id": "a1"})).unwrap();
        assert_eq!((d.tag.as_str(), d.text.as_str()), (SUBAGENT_STOP, "Sub-agent Explore finished (agent a1)"));
        assert!(event_draft("Stop", &json!({})).is_none());
        assert!(d.files.is_empty() && d.links.is_empty());

        let mut d = tool_drafts("Bash", &json!({"command": "cargo build"}), &json!({"stdout": "Finished"}), "/p");
        d.iter_mut().for_each(|x| x.ctx = "Turn 1 request: build".into());
        took(&mut d, 0);
        assert_eq!(took_of(&d[0].ctx), 0);
        took(&mut d, 42);
        assert_eq!(d[0].ctx, "Turn 1 request: build\nTook: 42s");
        assert_eq!(took_of(&d[0].ctx), 42);
        assert_eq!(took_of(&d[1].ctx), 0, "only the call carries it");
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

    #[test]
    fn decisions_and_memory_files_are_kept() {
        let d = prompt_drafts("My decision: customers win, totals round half up. The ledger export applies banker's rounding itself.");
        let dec: Vec<&Draft> = d.iter().filter(|x| x.kind == Kind::Decision).collect();
        assert_eq!(dec.len(), 1, "{d:?}");
        assert!(dec[0].text.starts_with("The user decided: My decision: customers win") && dec[0].text.contains("ledger export"), "{}", dec[0].text);
        assert!(prompt_drafts_by("Decision: ship it.", &Source::Agent("lead".into())).iter().all(|x| x.kind == Kind::Prompt));
        let input = serde_json::json!({"file_path": "/tmp/claude/memory/team/silo/money-rounding-half-up.md", "content": "---\nname: rounding\ntype: decision\n---\nMaya decided customers win: to_cents rounds half up.\n"});
        let t = tool_drafts("Write", &input, &serde_json::json!({}), "/repo");
        let n: Vec<&Draft> = t.iter().filter(|x| x.tag == MEMORY_FILE_TAG).collect();
        assert_eq!(n.len(), 1, "{t:?}");
        assert!(n[0].text.contains("/tmp/claude/memory/team/silo/money-rounding-half-up.md") && n[0].text.contains("Maya decided customers win"), "{}", n[0].text);
        assert!(!n[0].text.contains("type: decision"), "{}", n[0].text);
        let inside = serde_json::json!({"file_path": "/repo/docs/memory/notes.md", "content": "Some notes about the memory layout of the parser."});
        assert!(tool_drafts("Write", &inside, &serde_json::json!({}), "/repo").iter().all(|x| x.tag != MEMORY_FILE_TAG));
    }

    #[test]
    fn one_off_instructions_are_not_standing_rules() {
        // Maya's instruction to one agent, and what the distiller made of it.
        let req = "Fix the cent bug in to_cents. Don't touch money.py or cart.py beyond that, and don't touch web/. Commit on your branch when done.";
        assert!(!standing_rule(req, "", "money.py and cart.py must not be touched in pay or checkout bug-fix work."));
        // Said as a standing rule.
        assert!(standing_rule("From now on, never touch the migrations folder without asking me.", "", "The migrations folder must never be changed without asking the user."));
        assert!(standing_rule("Our convention: money is integer cents everywhere.", "", "Money is kept as integer cents everywhere (team convention)."));
        // "always" about something else does not make the instruction standing.
        assert!(!standing_rule("Tests always pass on main. Don't touch cart.py in this task.", "", "cart.py must not be changed in checkout work."));
        // The answer to a question that asked for a rule.
        assert!(standing_rule("Yes, do that.", "I rounded half up. Should I always round half up from now on?", "Amounts are always rounded half up."));
        assert!(!standing_rule("Yes, do that.", "Shall I commit it?", "Commits go on the agent's branch."));
    }
}
