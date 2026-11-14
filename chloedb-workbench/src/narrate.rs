//! Narration: one live sentence per agent, read from the tail of its
//! session's transcript (Claude Code's JSONL, the `transcript_path` of its
//! hooks), and the transcript helpers the story uses (assistant texts with
//! their times, tokens summed from `usage`). Nothing here calls a model.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{json, Value as Json};

/// The most of a transcript's end that narration reads.
pub const TAIL: u64 = 256 * 1024;
/// The most of a transcript the story reads, from its end.
pub const STORY_MAX: u64 = 64 << 20;
/// The longest sentence.
pub const MAX_CHARS: usize = 160;

/// Whether a hook's `transcript_path` looks like one: an absolute path of
/// a `.jsonl` file that is there.
pub fn transcript_ok(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute() && path.ends_with(".jsonl") && p.is_file()
}

/// The lines of the last `max` bytes of a JSONL file, parsed (a line cut by
/// the start of the window is left out).
pub fn read_tail(path: &Path, max: u64) -> Vec<Json> {
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if f.take(max).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines.iter().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// The content blocks of an assistant entry (`{"type": "assistant",
/// "message": {"content": [...]}}`).
fn assistant_blocks(e: &Json) -> Vec<&Json> {
    if e["type"] != "assistant" {
        return Vec::new();
    }
    match &e["message"]["content"] {
        Json::Array(a) => a.iter().collect(),
        _ => Vec::new(),
    }
}

/// Whether an entry is a person's prompt (not a tool's result, not meta).
pub fn is_prompt(e: &Json) -> bool {
    if e["type"] != "user" || e["isMeta"] == true {
        return false;
    }
    match &e["message"]["content"] {
        Json::String(s) => !s.trim().is_empty(),
        Json::Array(a) => a.iter().any(|b| b["type"] == "text") && !a.iter().any(|b| b["type"] == "tool_result"),
        _ => false,
    }
}

/// Markdown out of a line: headings, emphasis, code ticks, links, list marks.
pub fn strip_markdown(s: &str) -> String {
    let mut t = s.trim().trim_start_matches('#').trim_start().to_string();
    for p in ["- ", "* ", "+ ", "> "] {
        if let Some(r) = t.strip_prefix(p) {
            t = r.to_string();
        }
    }
    // A numbered list's mark: "1. ".
    if let Some((n, rest)) = t.split_once(". ") {
        if !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()) {
            t = rest.to_string();
        }
    }
    // Links: [text](url) → text.
    let mut out = String::with_capacity(t.len());
    let mut rest = t.as_str();
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find("](").and_then(|j| after[j + 2..].find(')').map(|k| (j, j + 2 + k))) {
            Some((j, end)) => {
                out.push_str(&after[..j]);
                rest = &after[end + 1..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace("__", "").replace('`', "").replace('*', "")
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out = out.trim_end().to_string();
    out.push('…');
    out
}

/// The first `n` sentences of a text: its first lines with words, Markdown
/// stripped, cut after `.`, `!` or `?` followed by a space (or at a line's
/// end), at most `max` characters.
pub fn sentences(text: &str, n: usize, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    'lines: for line in text.lines().map(strip_markdown).filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("```")) {
        let line = line.trim().to_string();
        let chars: Vec<char> = line.chars().collect();
        let mut start = 0;
        for i in 0..chars.len() {
            let end_here = matches!(chars[i], '.' | '!' | '?') && (i + 1 == chars.len() || chars[i + 1] == ' ');
            if end_here {
                let s: String = chars[start..=i].iter().collect();
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(s.trim());
                count += 1;
                start = i + 1;
                if count >= n {
                    break 'lines;
                }
            }
        }
        if start < chars.len() {
            let s: String = chars[start..].iter().collect();
            if !s.trim().is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(s.trim());
                count += 1;
            }
        }
        if count >= n {
            break;
        }
    }
    clip(&out, max)
}

/// A path for people: relative to `cwd` when inside it.
pub fn rel(path: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    match path.strip_prefix(cwd).and_then(|r| r.strip_prefix('/')) {
        Some(r) if !cwd.is_empty() => r.to_string(),
        _ => path.to_string(),
    }
}

fn dir_of(p: &str) -> &str {
    p.rsplit_once('/').map_or("", |(d, _)| d)
}

/// The folder several paths share (`""` when none).
pub fn common_dir(paths: &[String]) -> String {
    let mut it = paths.iter();
    let Some(first) = it.next() else { return String::new() };
    let mut common = dir_of(first).to_string();
    for p in it {
        while !common.is_empty() && !(p.starts_with(&common) && p[common.len()..].starts_with('/')) {
            common = dir_of(&common).to_string();
        }
    }
    common
}

fn input_str<'a>(input: &'a Json, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|k| input.get(*k).and_then(Json::as_str)).unwrap_or("")
}

/// A sentence made from tool calls at the end of a turn: the last one, or
/// for consecutive reads "Reading 4 files in src/refunds".
pub fn tool_sentence(calls: &[(String, Json)], cwd: &str) -> String {
    let Some((name, input)) = calls.last() else { return String::new() };
    let file = |i: &Json| rel(input_str(i, &["file_path", "notebook_path", "path"]), cwd);
    let s = match name.as_str() {
        "Read" | "NotebookRead" => {
            let reads: Vec<String> = calls.iter().rev().take_while(|(n, _)| n == "Read" || n == "NotebookRead").map(|(_, i)| file(i)).collect();
            if reads.len() > 1 {
                let d = common_dir(&reads);
                if d.is_empty() {
                    format!("Reading {} files", reads.len())
                } else {
                    format!("Reading {} files in {d}", reads.len())
                }
            } else {
                format!("Reading {}", file(input))
            }
        }
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => format!("Editing {}", file(input)),
        "Bash" => format!("Running `{}`", clip(input_str(input, &["command"]).lines().next().unwrap_or(""), 80)),
        "Grep" => format!("Searching for `{}`", clip(input_str(input, &["pattern"]), 60)),
        "Glob" => format!("Looking for files matching `{}`", clip(input_str(input, &["pattern"]), 60)),
        "LS" => format!("Listing {}", file(input)),
        "WebFetch" => format!("Reading {}", clip(input_str(input, &["url"]), 80)),
        "WebSearch" => format!("Searching the web for “{}”", clip(input_str(input, &["query"]), 60)),
        "Task" | "Agent" => format!("Delegating: {}", clip(input_str(input, &["description", "prompt"]), 80)),
        "TodoWrite" => "Updating its plan".into(),
        "mcp__workbench__spawn_agent" => format!("Starting an agent{}", match input_str(input, &["name"]) {
            "" => String::new(),
            n => format!(" ({n})"),
        }),
        "mcp__workbench__wait_agent" => "Waiting for an agent".into(),
        "mcp__workbench__ask_human" => format!("Asking you: {}", clip(input_str(input, &["question"]), 120)),
        n if n.starts_with("mcp__chloectx__") => "Checking memory".into(),
        n => match n.strip_prefix("mcp__").and_then(|r| r.rsplit_once("__")) {
            Some((server, tool)) => format!("Using {tool} ({server})"),
            None => format!("Using {n}"),
        },
    };
    clip(&s, MAX_CHARS)
}

/// The sentence of a transcript's tail: the first sentence of the latest
/// assistant text, or, when tool calls came after it, one made from them.
pub fn narrate(entries: &[Json], cwd: &str) -> Option<String> {
    let mut calls: Vec<(String, Json)> = Vec::new();
    for e in entries.iter().rev() {
        if is_prompt(e) {
            break;
        }
        for b in assistant_blocks(e).into_iter().rev() {
            match b["type"].as_str() {
                Some("text") => {
                    let t = b["text"].as_str().unwrap_or("");
                    if t.trim().is_empty() {
                        continue;
                    }
                    if calls.is_empty() {
                        let s = sentences(t, 1, MAX_CHARS);
                        if !s.is_empty() {
                            return Some(s);
                        }
                    } else {
                        calls.reverse();
                        return Some(tool_sentence(&calls, cwd));
                    }
                }
                Some("tool_use") => calls.push((b["name"].as_str().unwrap_or("").to_string(), b["input"].clone())),
                _ => {}
            }
        }
    }
    if calls.is_empty() {
        return None;
    }
    calls.reverse();
    Some(tool_sentence(&calls, cwd))
}

/// The tokens of a transcript: `usage` summed once per message (Claude
/// Code writes a line per content block, each with the message's usage).
pub fn tokens(entries: &[Json], seen: &mut HashSet<String>) -> (i64, i64, i64) {
    let (mut i, mut o, mut c) = (0, 0, 0);
    for e in entries.iter().filter(|e| e["type"] == "assistant") {
        let m = &e["message"];
        let key = m["id"].as_str().or_else(|| e["requestId"].as_str()).or_else(|| e["uuid"].as_str()).unwrap_or("").to_string();
        if !key.is_empty() && !seen.insert(key) {
            continue;
        }
        let u = &m["usage"];
        let n = |k: &str| u[k].as_i64().unwrap_or(0);
        i += n("input_tokens") + n("cache_creation_input_tokens");
        o += n("output_tokens");
        c += n("cache_read_input_tokens");
    }
    (i, o, c)
}

/// The assistant texts of a transcript, with their times (ms).
pub fn assistant_texts(entries: &[Json]) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for e in entries {
        let ts = e["timestamp"].as_str().and_then(chloedb::timestamp::parse).map(|t| t.div_euclid(1000)).unwrap_or(0);
        for b in assistant_blocks(e) {
            if b["type"] == "text" {
                if let Some(t) = b["text"].as_str().filter(|t| !t.trim().is_empty()) {
                    out.push((ts, t.to_string()));
                }
            }
        }
    }
    out
}

/// The tokens as the API answers them.
pub fn tokens_json(t: (i64, i64, i64)) -> Json {
    json!({ "input": t.0, "output": t.1, "cache_read": t.2 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asst(blocks: Json) -> Json {
        json!({ "type": "assistant", "message": { "id": "m1", "content": blocks } })
    }

    #[test]
    fn sentences_strip_markdown_and_stop_at_the_first() {
        assert_eq!(sentences("**Found** two refund paths: the [API](http://x) and the batch. Then more.", 1, 160), "Found two refund paths: the API and the batch.");
        assert_eq!(sentences("## Plan\n- Fix `post.rs` first. Then tests.", 2, 160), "Plan Fix post.rs first.");
        assert_eq!(sentences("one. two. three.", 2, 160), "one. two.");
        assert_eq!(sentences(&"word ".repeat(100), 1, 20).chars().count(), 20);
        assert_eq!(sentences("Version 1.2 is out", 1, 160), "Version 1.2 is out");
    }

    #[test]
    fn a_tool_call_after_the_text_makes_the_sentence() {
        let cwd = "/w";
        let e = vec![
            json!({ "type": "user", "message": { "content": "go" } }),
            asst(json!([{ "type": "text", "text": "Looking at refunds now. More." }])),
        ];
        assert_eq!(narrate(&e, cwd).unwrap(), "Looking at refunds now.");
        let mut e2 = e.clone();
        for f in ["a.rs", "b.rs", "sub/c.rs", "sub/d.rs"] {
            e2.push(asst(json!([{ "type": "tool_use", "name": "Read", "input": { "file_path": format!("/w/src/refunds/{f}") } }])));
            e2.push(json!({ "type": "user", "message": { "content": [{ "type": "tool_result", "content": "x" }] } }));
        }
        assert_eq!(narrate(&e2, cwd).unwrap(), "Reading 4 files in src/refunds");
        e2.push(asst(json!([{ "type": "tool_use", "name": "Bash", "input": { "command": "cargo test -p harbor\nmore" } }])));
        assert_eq!(narrate(&e2, cwd).unwrap(), "Running `cargo test -p harbor`");
        e2.push(asst(json!([{ "type": "tool_use", "name": "Edit", "input": { "file_path": "/w/src/ledger/post.rs" } }])));
        assert_eq!(narrate(&e2, cwd).unwrap(), "Editing src/ledger/post.rs");
        // Nothing of the assistant's since the last prompt: nothing to say.
        let e3 = vec![json!({ "type": "user", "message": { "content": "hello" } })];
        assert_eq!(narrate(&e3, cwd), None);
    }

    #[test]
    fn tokens_count_a_message_once() {
        let u = json!({ "input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 2 });
        let e = vec![
            json!({ "type": "assistant", "message": { "id": "m1", "usage": u, "content": [] } }),
            json!({ "type": "assistant", "message": { "id": "m1", "usage": u, "content": [] } }),
            json!({ "type": "assistant", "message": { "id": "m2", "usage": u, "content": [] } }),
        ];
        assert_eq!(tokens(&e, &mut HashSet::new()), (24, 10, 200));
    }

    #[test]
    fn tails_skip_a_cut_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let mut body = String::new();
        for i in 0..100 {
            body.push_str(&format!("{{\"n\":{i},\"pad\":\"{}\"}}\n", "x".repeat(50)));
        }
        std::fs::write(&p, &body).unwrap();
        let t = read_tail(&p, 200);
        assert!(!t.is_empty() && t.len() <= 3);
        assert_eq!(t.last().unwrap()["n"], 99);
        assert_eq!(read_tail(&p, 1 << 20).len(), 100);
        assert_eq!(common_dir(&["src/a/x.rs".into(), "src/a/b/y.rs".into()]), "src/a");
    }
}
