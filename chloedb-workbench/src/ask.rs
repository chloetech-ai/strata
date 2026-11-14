//! Ask about a project (`POST /api/ask`), answered for a person: what
//! ChloeCtx remembers that is about the question (each note phrased as a
//! sentence with who said it and when, never the context block agents get),
//! where the code holds it (the Search part's BM25, definitions first), and
//! when and why it changed (Trace's reading of git). No model is called.
//!
//! The reply keeps what `chloectx ask --json` printed (`text`, `ids`,
//! `answer`) and `agents`, for the Decisions timeline, and adds `summary`,
//! `memory`, `code`, `history` and `took_ms` (whole milliseconds).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use crate::search::split_ident;
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::trail::{self, Ran, Sessions};

/// Words a question is made of that find nothing.
const STOP: &[&str] = &[
    "the", "and", "for", "with", "not", "but", "you", "your", "this", "that", "what", "which", "how", "why", "when", "where", "who", "does", "did", "are",
    "was", "were", "is", "it", "its", "of", "to", "in", "on", "at", "by", "an", "be", "do", "we", "our", "us", "from", "into", "about", "there", "here",
    "now", "can", "could", "should", "would", "will", "has", "have", "had", "any", "all", "one", "get", "got", "use", "used", "make", "made", "work",
    "works", "code", "file", "project", "change", "changed",
];

/// The words of a question worth looking for, lower case.
pub fn terms(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in q.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '/' || c == '-')) {
        let w = w.trim_matches(|c: char| c == '.' || c == '-' || c == '/');
        if w.is_empty() {
            continue;
        }
        let lw = w.to_lowercase();
        let parts: Vec<String> = if lw.contains('.') || lw.contains('/') { vec![lw.clone()] } else { split_ident(w) };
        for p in parts.into_iter().chain(std::iter::once(lw)) {
            if p.len() > 2 && !STOP.contains(&p.as_str()) && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// How many of `terms` a text holds (a term matches a word that starts
/// with its first five letters, so "rounding" finds "round", "totals" "total").
pub fn overlap(terms: &[String], text: &str) -> usize {
    let words: HashSet<String> = split_ident(text).into_iter().chain(text.to_lowercase().split_whitespace().map(str::to_string)).collect();
    terms
        .iter()
        .filter(|t| {
            let stem: String = t.chars().take(5).collect();
            words.iter().any(|w| w == *t || (stem.len() >= 4 && w.starts_with(&stem)) || (w.len() >= 4 && t.starts_with(w.as_str())))
        })
        .count()
}

/// A ChloeCtx note as a person reads it: who said it (an agent, or you)
/// and the sentence, without "The agent X said:" or "My decision:".
pub fn phrase(text: &str) -> (Option<String>, bool, String) {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for pre in ["The agent ", "Agent "] {
        if let Some(rest) = t.strip_prefix(pre) {
            if let Some((name, said)) = rest.split_once(" said: ").or_else(|| rest.split_once(" said ")) {
                if !name.contains(' ') {
                    return (Some(name.to_string()), false, said.trim().to_string());
                }
            }
        }
    }
    let mut you = false;
    let mut s = t.as_str();
    for pre in ["The user decided: ", "The user said: ", "User preference: ", "My decision: ", "my decision: "] {
        if let Some(rest) = s.strip_prefix(pre) {
            you = true;
            s = rest;
        }
    }
    // Workbench's "(About chapter 2, …)" prefix of a steer is not the user's words.
    if s.starts_with("(About ") {
        if let Some(i) = s.find(") ") {
            s = &s[i + 2..];
        }
    }
    (None, you, s.trim().to_string())
}

/// `POST /api/ask`.
pub fn answer(sup: &Arc<Supervisor>, repo: Option<&str>, question: &str) -> Result<Json, ApiError> {
    let started = Instant::now();
    // As before: ChloeCtx's own reply (text, ids, agents), which the
    // Decisions timeline reads.
    let mut out = trail::ask(sup, repo, question)?;
    let dir = trail::repo_dir(repo)?;
    let repo_s = dir.to_string_lossy().to_string();
    let ts = terms(question);
    let mut missing: Vec<String> = out["missing"].as_array().cloned().unwrap_or_default().into_iter().filter_map(|m| m.as_str().map(str::to_string)).collect();

    // Memory: the records ChloeCtx picked, read back as rows, kept only
    // when they share words with the question.
    let ids: Vec<String> = out["ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)).collect();
    let mut memory: Vec<(usize, Json)> = Vec::new();
    if !ids.is_empty() {
        let list: Vec<String> = ids.iter().take(40).map(|i| format!("'{}'", i.replace('\'', "''"))).collect();
        let sql = format!("SELECT id, kind, ts, session, text, files, tag FROM memory WHERE id IN ({})", list.join(", "));
        match trail::run_chloectx(&sup.cfg.chloectx_bin, &dir, &["sql", "--json", &sql], Duration::from_secs(60)) {
            Ran::Out(v) => {
                let cols: Vec<String> = v["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
                let s = Sessions::load(sup);
                for row in v["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
                    let col = |n: &str| cols.iter().position(|c| c == n).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
                    let kind = col("kind").as_str().unwrap_or("note").to_string();
                    if col("tag") == "superseded" {
                        continue;
                    }
                    let raw = col("text").as_str().unwrap_or("").to_string();
                    let files = trail::files_of(&col("files"));
                    let score = overlap(&ts, &raw) + overlap(&ts, &files.join(" "));
                    if score == 0 {
                        continue;
                    }
                    let transcript = matches!(kind.as_str(), "prompt" | "answer" | "episode");
                    let (agent_said, you, text) = phrase(&raw);
                    let id = col("id").as_str().unwrap_or("").to_string();
                    let agent = col("session").as_str().and_then(|sid| s.agent(sid)).or_else(|| s.of_record(&id));
                    let who = if you || kind == "prompt" {
                        "you".to_string()
                    } else if let Some(n) = agent_said.clone().or_else(|| agent.map(|a| a.name.clone())) {
                        n
                    } else {
                        "an earlier session".to_string()
                    };
                    let what = match kind.as_str() {
                        "decision" => "Decided",
                        "preference" | "rule" => "Rule",
                        "fact" => "Noted",
                        "error" | "fix" => "Fixed before",
                        "prompt" => "You asked",
                        "answer" | "episode" => "Said in a session",
                        _ => "Noted",
                    };
                    memory.push((
                        score + if transcript { 0 } else { 1 },
                        json!({
                            "id": id, "kind": kind, "what": what, "who": who,
                            "agent": agent.map(|a| json!({ "id": a.id, "name": a.name })),
                            "text": if transcript { one_line(&text, 240) } else { text },
                            "ts": trail::ctx_time(&col("ts")), "ts_ms": trail::ctx_ms(&col("ts")),
                            "files": files, "transcript": transcript,
                        }),
                    ));
                }
            }
            Ran::Missing => {}
            Ran::Failed(e) => missing.push(e),
        }
    }
    let best = memory.iter().map(|m| m.0).max().unwrap_or(0);
    memory.retain(|m| m.0 * 2 >= best);
    memory.sort_by(|a, b| b.0.cmp(&a.0).then(b.1["ts_ms"].as_i64().cmp(&a.1["ts_ms"].as_i64())));
    // Notes before transcripts; at most one excerpt when there are notes.
    let notes: Vec<Json> = memory.iter().filter(|m| m.1["transcript"] != true).map(|m| m.1.clone()).take(5).collect();
    let excerpts: Vec<Json> = memory.iter().filter(|m| m.1["transcript"] == true).map(|m| m.1.clone()).take(if notes.is_empty() { 3 } else { 1 }).collect();
    let memory: Vec<Json> = notes.into_iter().chain(excerpts).collect();

    // Code: the Search part's hits for the question's words.
    let words = ts.join(" ");
    let mut code: Vec<Json> = Vec::new();
    if !words.is_empty() {
        match crate::search::quick(sup, Some(&repo_s), &words) {
            Ok(v) => {
                for m in v["missing"].as_array().into_iter().flatten().filter_map(Json::as_str) {
                    missing.push(m.to_string());
                }
                for c in v["code"].as_array().into_iter().flatten().take(5) {
                    let text = c["text"].as_str().unwrap_or("");
                    let lines: Vec<&str> = text.lines().collect();
                    let start = c["start_line"].as_i64().unwrap_or(1);
                    // The first line that holds a term, and a few around it.
                    let at = lines.iter().position(|l| overlap(&ts, l) > 0).unwrap_or(0);
                    let from = at.saturating_sub(1);
                    let snippet: Vec<&str> = lines.iter().skip(from).take(6).copied().collect();
                    code.push(json!({
                        "path": c["path"], "symbol": c["symbol"], "kind": c["kind"],
                        "start_line": c["start_line"], "end_line": c["end_line"],
                        "line": start + at as i64, "snippet_from": start + from as i64, "snippet": snippet.join("\n"),
                    }));
                }
            }
            Err(e) => missing.push(e.message),
        }
    }

    // History: Trace's reading of git, without a model.
    let tq = crate::trace::TraceQ { q: Some(question.to_string()), repo: Some(repo_s.clone()), agent: None, x: None, y: None, topic: None, subject: None, in_: None, kinds: None };
    let mut history = json!({ "answer": null, "commits": [] });
    if let Ok(t) = crate::trace::trace(sup, &tq) {
        let commits: Vec<Json> = t["timeline"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["kind"] == "commit")
            .rev()
            .take(4)
            .map(|i| json!({ "sha": i["commit"]["sha"], "short": i["commit"]["short"], "subject": i["commit"]["subject"], "by": i["by"], "ts": i["ts"], "ts_ms": i["ts_ms"], "files": i["commit"]["files"] }))
            .collect();
        history = json!({ "answer": t["answer"]["plain"], "commits": commits });
    }

    // One sentence first: the history's when-and-why, else the closest
    // note, else where the code is.
    let summary = if let Some(a) = history["answer"].as_str() {
        a.to_string()
    } else if let Some(m) = memory.iter().find(|m| m["transcript"] != true) {
        format!("{} ({}): {}", m["what"].as_str().unwrap_or("Noted"), m["who"].as_str().unwrap_or(""), one_line(m["text"].as_str().unwrap_or(""), 220))
    } else if let Some(c) = code.first() {
        let sym = c["symbol"].as_str().map(|s| format!("{s} in ")).unwrap_or_default();
        format!("Look at {sym}{}:{}. ChloeCtx has no note about this yet.", c["path"].as_str().unwrap_or(""), c["line"])
    } else {
        "Nothing in memory, the code or its history matches these words. Name a file, a function or a value.".to_string()
    };

    out["summary"] = json!(summary);
    out["memory"] = json!(memory);
    out["code"] = json!(code);
    out["history"] = history;
    out["terms"] = json!(ts);
    out["missing"] = json!(missing);
    out["took_ms"] = json!(started.elapsed().as_millis() as u64);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_is_read_for_its_words() {
        assert_eq!(terms("where is money formatted?"), ["money", "formatted"]);
        assert_eq!(terms("why does fmt in money.py round half up"), ["fmt", "money.py", "round", "half"]);
        assert_eq!(overlap(&terms("why were checkout totals a cent off?"), "Tax on the checkout total rounds half up"), 2);
        assert_eq!(overlap(&terms("how is rounding done"), "to_cents rounds half to even"), 1);
        assert_eq!(overlap(&terms("where is money formatted"), "The Pay button calls undefinedFn"), 0);
    }

    #[test]
    fn notes_are_phrased_for_a_person() {
        assert_eq!(phrase("The agent centoff said: to_cents keeps half-even"), (Some("centoff".into()), false, "to_cents keeps half-even".into()));
        assert_eq!(phrase("The user decided: My decision: keep every test"), (None, true, "keep every test".into()));
        assert_eq!(phrase("User preference: (About chapter 3, Tests.) From now on, ignore test_promo_window"), (None, true, "From now on, ignore test_promo_window".into()));
        assert_eq!(phrase("Totals round half up"), (None, false, "Totals round half up".into()));
    }
}
