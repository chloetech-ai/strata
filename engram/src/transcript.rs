//! Replays a Claude Code transcript (JSONL) into the records the hooks
//! would have stored, for back-filling history and for evaluation. The
//! transcript format is internal to Claude Code, so parsing is tolerant:
//! unknown entries are skipped.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

use serde_json::Value as Json;

use crate::capture::{self, EpisodeAcc};
use crate::record::Draft;
use crate::Result;

/// Unix seconds of an RFC 3339 UTC timestamp (`2026-10-11T05:00:50.285Z`).
pub fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// What a hook would have seen, in order.
#[derive(Clone, Debug)]
pub enum Event {
    Prompt { text: String },
    Tool { name: String, input: Json, response: Json },
    Stop { answer: String },
    Compact { summary: String },
    /// Assistant text between tool calls (not the turn's final message).
    Narration { text: String },
}

#[derive(Clone, Debug)]
pub struct Timed {
    pub line: usize,
    pub ts: i64,
    pub session: String,
    pub event: Event,
}

fn text_of(content: &Json) -> String {
    match content {
        Json::String(s) => s.clone(),
        Json::Array(a) => a
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// System-generated "user" messages that are not prompts.
fn is_synthetic(text: &str) -> bool {
    let t = text.trim_start();
    t.is_empty()
        || t.starts_with("<task-notification>")
        || t.starts_with("<command-")
        || t.starts_with("<local-command")
        || t.starts_with("<system-reminder>")
        || t.starts_with("Caveat:")
        || t.starts_with("[Request interrupted")
}

/// Parses a transcript into hook-like events.
pub fn read(path: &Path) -> Result<Vec<Timed>> {
    let f = std::fs::File::open(path)?;
    let mut out = Vec::new();
    let mut calls: HashMap<String, (String, Json)> = HashMap::new();
    let mut last_answer = String::new();
    let mut in_turn = false;
    let mut last_ts = 0;
    let mut session = String::new();
    for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
        let Ok(line) = line else { continue };
        let Ok(d) = serde_json::from_str::<Json>(&line) else { continue };
        let ts = d.get("timestamp").and_then(|t| t.as_str()).and_then(parse_ts).unwrap_or(last_ts);
        last_ts = ts;
        if let Some(s) = d.get("sessionId").and_then(|s| s.as_str()) {
            session = s.to_string();
        }
        let ty = d.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let msg = d.get("message");
        let content = msg.and_then(|m| m.get("content")).cloned().unwrap_or(Json::Null);
        let push = |out: &mut Vec<Timed>, e: Event| out.push(Timed { line: i + 1, ts, session: session.clone(), event: e });
        match ty {
            "user" => {
                if d.get("isCompactSummary").and_then(|v| v.as_bool()) == Some(true) {
                    if in_turn {
                        push(&mut out, Event::Stop { answer: std::mem::take(&mut last_answer) });
                        in_turn = false;
                    }
                    push(&mut out, Event::Compact { summary: text_of(&content) });
                    continue;
                }
                if let Json::Array(blocks) = &content {
                    let mut had_result = false;
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                            had_result = true;
                            let id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                            if let Some((name, input)) = calls.remove(id) {
                                let mut response = d.get("toolUseResult").cloned().filter(|r| !r.is_null()).unwrap_or_else(|| b.get("content").cloned().unwrap_or(Json::Null));
                                if b.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                                    response = serde_json::json!({"is_error": true, "error": capture::response_text(&b.get("content").cloned().unwrap_or(Json::Null))});
                                }
                                push(&mut out, Event::Tool { name, input, response });
                            }
                        }
                    }
                    if had_result {
                        continue;
                    }
                }
                if d.get("isMeta").and_then(|v| v.as_bool()) == Some(true) {
                    continue;
                }
                let text = text_of(&content);
                if is_synthetic(&text) {
                    continue;
                }
                if in_turn {
                    push(&mut out, Event::Stop { answer: std::mem::take(&mut last_answer) });
                }
                push(&mut out, Event::Prompt { text });
                in_turn = true;
            }
            "assistant" => {
                if let Json::Array(blocks) = &content {
                    for b in blocks {
                        match b.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                let t = b.get("text").and_then(|t| t.as_str()).unwrap_or("").trim();
                                if !t.is_empty() {
                                    last_answer = t.to_string();
                                    push(&mut out, Event::Narration { text: t.to_string() });
                                }
                            }
                            Some("tool_use") => {
                                let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                calls.insert(id, (name, b.get("input").cloned().unwrap_or(Json::Null)));
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if in_turn {
        out.push(Timed { line: usize::MAX, ts: last_ts, session, event: Event::Stop { answer: last_answer } });
    }
    Ok(out)
}

/// A batch of drafts for one session turn at one time.
#[derive(Clone, Debug)]
pub struct Batch {
    pub line: usize,
    pub session: String,
    pub turn: i64,
    pub ts: i64,
    pub drafts: Vec<Draft>,
    /// The batch is a compaction summary.
    pub compact: bool,
}

/// The drafts the hooks would have stored for these events, in order.
pub fn drafts(events: &[Timed], project: &str) -> Vec<Batch> {
    let mut out: Vec<Batch> = Vec::new();
    let mut turn = 0i64;
    let mut ep = EpisodeAcc::default();
    // Narration batches of the current turn: (index in out, text).
    let mut narr: Vec<(usize, String)> = Vec::new();
    for e in events {
        if let Event::Stop { answer } = &e.event {
            // The final message is stored as the answer, not as narration.
            if let Some((i, _)) = narr.iter().rev().find(|(_, t)| t == answer) {
                out[*i].drafts.clear();
            }
            narr.clear();
        }
        let ctx = || format!("Turn {turn} request: {}", ep.prompt.chars().take(200).collect::<String>());
        let (drafts, compact) = match &e.event {
            Event::Prompt { text } => {
                turn += 1;
                ep = EpisodeAcc { prompt: text.clone(), ..Default::default() };
                (capture::prompt_drafts(text), false)
            }
            Event::Tool { name, input, response } => {
                let c = ctx();
                ep.add_tool(name, input, response, project);
                (capture::tool_drafts(name, input, response, project).into_iter().map(|d| d.ctx(c.clone())).collect(), false)
            }
            Event::Stop { answer } => {
                let c = ctx();
                let mut d: Vec<Draft> = capture::answer_drafts(answer).into_iter().map(|d| d.ctx(c.clone())).collect();
                d.extend(ep.card(turn, answer));
                (d, false)
            }
            Event::Compact { summary } => (capture::summary_drafts(summary), true),
            Event::Narration { text } => {
                let c = ctx();
                narr.push((out.len(), text.clone()));
                (capture::narration_drafts(text).into_iter().map(|d| d.ctx(c.clone())).collect(), false)
            }
        };
        if !drafts.is_empty() {
            out.push(Batch { line: e.line, session: e.session.clone(), turn, ts: e.ts, drafts, compact });
        } else if matches!(e.event, Event::Narration { .. }) {
            narr.pop();
        }
    }
    out.retain(|b| !b.drafts.is_empty());
    out
}

/// Assistant text blocks in transcript lines after `offset` bytes (for the
/// live daemon's narration capture). Returns the texts and the new offset.
pub fn tail_texts(path: &Path, offset: u64) -> Result<(Vec<String>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if offset >= len {
        return Ok((Vec::new(), len.min(offset)));
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.take(64 << 20).read_to_end(&mut buf)?;
    // Only complete lines; a partial last line is read next time.
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut out = Vec::new();
    for line in buf[..end].split(|&b| b == b'\n') {
        let Ok(d) = serde_json::from_slice::<Json>(line) else { continue };
        if d.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(Json::Array(blocks)) = d.pointer("/message/content") {
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    let t = b.get("text").and_then(|t| t.as_str()).unwrap_or("").trim();
                    if !t.is_empty() {
                        out.push(t.to_string());
                    }
                }
            }
        }
    }
    Ok((out, offset + end as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2026-10-11T05:00:50.285Z"), Some(1_791_694_850));
        assert_eq!(crate::recall::fmt_time(parse_ts("2024-02-29T13:05:00Z").unwrap()), "2024-02-29 13:05 UTC");
    }

    #[test]
    fn replays_a_small_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let lines = [
            r#"{"type":"user","sessionId":"s1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"Fix the parser. Always run the tests first."}}"#,
            r#"{"type":"assistant","sessionId":"s1","timestamp":"2026-01-01T00:00:05Z","message":{"content":[{"type":"text","text":"Running tests."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"user","sessionId":"s1","timestamp":"2026-01-01T00:00:09Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"test result: FAILED"}]},"toolUseResult":{"stdout":"test result: FAILED","stderr":""}}"#,
            r#"{"type":"assistant","sessionId":"s1","timestamp":"2026-01-01T00:00:12Z","message":{"content":[{"type":"text","text":"The parser test fails on empty input."}]}}"#,
            r#"{"type":"user","sessionId":"s1","timestamp":"2026-01-01T00:01:00Z","isCompactSummary":true,"message":{"content":"Summary:\n1. Primary Request and Intent:\n fix parser\n7. Pending Tasks:\n - empty input"}}"#,
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        let ev = read(&p).unwrap();
        let kinds: Vec<&str> = ev.iter().map(|e| match e.event { Event::Prompt { .. } => "p", Event::Tool { .. } => "t", Event::Stop { .. } => "s", Event::Compact { .. } => "c", Event::Narration { .. } => "n" }).collect();
        assert_eq!(kinds, ["p", "n", "t", "n", "s", "c"]);
        let b = drafts(&ev, "/p");
        let all: Vec<_> = b.iter().flat_map(|b| b.drafts.iter().map(|d| d.kind.as_str())).collect();
        assert!(all.contains(&"preference") && all.contains(&"error") && all.contains(&"episode") && all.contains(&"todo"), "{all:?}");
        assert!(b.last().unwrap().compact);
    }
}
