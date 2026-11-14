//! The flaky test radar (SPEC.md, "Fourth build", row 5): every test run an
//! agent makes, read from its Bash calls (a `PostToolUse`, a
//! `PostToolUseFailure`, or, for a call that failed and fired neither, the
//! transcript's `tool_result` at the turn's end), parsed test by test for
//! cargo test (and nextest), pytest, jest, vitest and go test, as rows of
//! `test_runs`; an unknown runner is one row for the whole command with its
//! exit code. From those rows: p50/p99 with `approx_percentile`, pass rates,
//! flaky tests (passed and failed on the same code, or flipping within a day
//! on one commit), newly failing and slow ones.
//!
//! [`after_hook`] is the one place the server calls after each hook: it also
//! hands file edits and transcript paths to [`crate::alerts`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_ms, get_str, iso, now_ms, opt_int, opt_str, ts};
use crate::server::{blocking, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};

/// The most tests one command records.
const MAX_TESTS: usize = 5000;
/// What a failed test keeps of its error.
const ERROR_MAX: usize = 2000;
/// How much of a transcript is read to find a failed call's output.
const TRANSCRIPT_TAIL: u64 = 16 << 20;

/// Sixth build: every test command as one row (its counts, exit code, time
/// and permission wait), `test_runs` keeping only tests by name. src/db.rs
/// makes both.
pub const SUMMARIES: &str = "CREATE TABLE IF NOT EXISTS test_summaries (id KEYWORD PRIMARY KEY, repo KEYWORD, agent KEYWORD, \"commit\" KEYWORD, tree KEYWORD, runner KEYWORD, command TEXT, tests INT, passed INT, failed INT, skipped INT, complete BOOL, exit_code INT, ms INT, wait_ms INT, source KEYWORD, use_id KEYWORD, ts TIMESTAMP, seq INT)";
pub const ADDED: &str = "ALTER TABLE test_runs ADD COLUMN IF NOT EXISTS wait_ms INT";

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/tests", get(tests_route))
        .route("/api/tests/history", get(history_route))
        .route("/api/tests/runs", get(runs_route))
        .route("/api/tests/rerun", axum::routing::post(rerun_route))
        .route("/api/tests/rerun/:id", get(rerun_get_route))
}

// ------------------------------------------------------------------ parsing

/// One test's result in a run.
#[derive(Clone, Debug, PartialEq)]
pub struct TestResult {
    pub name: String,
    /// `passed`, `failed` or `skipped`.
    pub outcome: &'static str,
    /// Its own time, when the runner reports it.
    pub ms: Option<i64>,
    pub error: Option<String>,
}

/// What a test command's output says.
#[derive(Debug, Default)]
pub struct Parsed {
    /// `cargo`, `nextest`, `pytest`, `jest`, `vitest`, `go`, or `other`.
    pub runner: String,
    pub tests: Vec<TestResult>,
    /// The run's own total, when it prints one.
    pub total_ms: Option<i64>,
    /// The run's summary line (passed, failed, skipped), when it prints
    /// one: all there is of a quiet run (`pytest -q` that passed).
    pub counts: Option<Counts>,
}

/// How many tests of a run passed, failed and were skipped.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub passed: i64,
    pub failed: i64,
    pub skipped: i64,
}

impl Counts {
    fn add(&mut self, o: Counts) {
        self.passed += o.passed;
        self.failed += o.failed;
        self.skipped += o.skipped;
    }
    fn of(tests: &[TestResult]) -> Counts {
        let n = |o: &str| tests.iter().filter(|t| t.outcome == o).count() as i64;
        Counts { passed: n("passed"), failed: n("failed"), skipped: n("skipped") }
    }
    pub fn total(&self) -> i64 {
        self.passed + self.failed + self.skipped
    }
}

/// "3 passed, 1 failed, 2 skipped": the numbers a summary puts before
/// `passed`, `failed`, `error(s)`, `skipped`, `ignored`, `xfailed`,
/// `xpassed` or `todo`; `None` when it names none.
fn counts_in(text: &str) -> Option<Counts> {
    let mut c = Counts::default();
    let mut any = false;
    let words: Vec<&str> = text.split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | '|' | '(' | ')')).filter(|w| !w.is_empty()).collect();
    for w in words.windows(2) {
        let Ok(n) = w[0].parse::<i64>() else { continue };
        match w[1].trim_end_matches('.').to_ascii_lowercase().as_str() {
            "passed" | "xpassed" => c.passed += n,
            "failed" | "error" | "errors" => c.failed += n,
            "skipped" | "ignored" | "xfailed" | "todo" | "pending" => c.skipped += n,
            _ => continue,
        }
        any = true;
    }
    any.then_some(c)
}

/// pytest's last line, with or without its `=`s ("8 passed in 0.30s",
/// "== 1 failed, 7 passed, 1 warning in 0.41s (0:00:00) =="): its counts
/// and time.
fn pytest_summary(line: &str) -> Option<(Counts, Option<i64>)> {
    let t = line.trim().trim_matches('=').trim();
    let (head, tail) = t.rsplit_once(" in ")?;
    let shaped = head.split(", ").all(|part| {
        let mut w = part.split_whitespace();
        w.next().is_some_and(|n| n.parse::<i64>().is_ok()) && w.next().is_some() && w.next().is_none()
    });
    if !shaped {
        return None;
    }
    let c = counts_in(head)?;
    Some((c, tail.split_whitespace().next().and_then(secs_ms)))
}

/// pytest's summary anywhere in a line, after whatever an `echo` put
/// before it ("run03 exit=1 ===== 1 failed, 7 passed in 0.04s =====").
fn pytest_summary_in(line: &str) -> Option<(Counts, Option<i64>)> {
    let t = line.trim();
    if let Some(s) = pytest_summary(t) {
        return Some(s);
    }
    if !t.contains(" in ") || !["passed", "failed", "error", "skipped", "deselected"].iter().any(|w| t.contains(w)) {
        return None;
    }
    let b = t.as_bytes();
    (1..b.len()).filter(|&i| b[i].is_ascii_digit() && matches!(b[i - 1], b' ' | b'=' | b'\t')).find_map(|i| pytest_summary(&t[i..]))
}

fn secs_ms(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches('(').trim_end_matches(')').trim_end_matches(',');
    if let Some(v) = s.strip_suffix("ms") {
        return v.trim().parse::<f64>().ok().map(|x| x.round() as i64);
    }
    let v = s.strip_suffix('s')?;
    v.trim().parse::<f64>().ok().map(|x| (x * 1000.0).round() as i64)
}

fn clip(lines: &[String]) -> Option<String> {
    let text = lines.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n");
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut end = text.len().min(ERROR_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(text[..end].to_string())
}

/// Adds a result, keeping the first of a name (summaries repeat them) but
/// letting a later failure or time fill in.
fn put(out: &mut Vec<TestResult>, seen: &mut HashMap<String, usize>, r: TestResult) {
    match seen.get(&r.name) {
        Some(&i) => {
            let had = &mut out[i];
            if r.outcome == "failed" {
                had.outcome = "failed";
            }
            if had.ms.is_none() {
                had.ms = r.ms;
            }
            if had.error.is_none() {
                had.error = r.error;
            }
        }
        None => {
            seen.insert(r.name.clone(), out.len());
            out.push(r);
        }
    }
}

/// cargo test's `test a::b ... ok <0.012s>` lines (and its failures'
/// stdout), and nextest's `PASS [ 0.012s] crate a::b`.
fn parse_cargo(text: &str) -> Parsed {
    let mut out = Vec::new();
    let mut seen = HashMap::new();
    let mut total = 0i64;
    let mut any_total = false;
    let mut nextest = false;
    let mut counts: Option<Counts> = None;
    let mut errors: HashMap<String, Vec<String>> = HashMap::new();
    let mut in_err: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        let t = line.trim_start();
        if let Some(name) = t.strip_prefix("---- ").and_then(|r| r.strip_suffix(" stdout ----").or_else(|| r.strip_suffix(" stderr ----"))) {
            in_err = Some(name.trim().to_string());
            continue;
        }
        if let Some(name) = &in_err {
            if t.starts_with("---- ") || t == "failures:" || t.starts_with("test result:") || t.starts_with("successes:") {
                in_err = None;
            } else {
                let e = errors.entry(name.clone()).or_default();
                if e.len() < 40 {
                    e.push(line.to_string());
                }
                continue;
            }
        }
        if let Some(rest) = t.strip_prefix("test ") {
            if let Some((name, res)) = rest.split_once(" ... ") {
                let word = res.split_whitespace().next().unwrap_or("");
                let outcome = match word {
                    "ok" => "passed",
                    "FAILED" => "failed",
                    w if w.starts_with("ignored") => "skipped",
                    _ => continue,
                };
                let ms = res.split_once('<').and_then(|(_, r)| r.split_once('>')).and_then(|(s, _)| secs_ms(s));
                put(&mut out, &mut seen, TestResult { name: name.trim().to_string(), outcome, ms, error: None });
                continue;
            }
        }
        if t.starts_with("test result:") {
            if let Some(ms) = t.split("finished in ").nth(1).and_then(secs_ms) {
                total += ms;
                any_total = true;
            }
            if let Some(c) = counts_in(t) {
                counts.get_or_insert_with(Counts::default).add(c);
            }
            continue;
        }
        // nextest: "PASS [   0.012s] crate tests::name"
        let mut w = t.split_whitespace();
        if let Some(status @ ("PASS" | "FAIL" | "SKIP" | "SIGSEGV" | "TIMEOUT" | "LEAK")) = w.next() {
            if let Some(after) = t.split_once('[').and_then(|(_, r)| r.split_once(']')) {
                let (secs, name) = after;
                let name = name.trim();
                if !name.is_empty() {
                    nextest = true;
                    let outcome = match status {
                        "PASS" | "LEAK" => "passed",
                        "SKIP" => "skipped",
                        _ => "failed",
                    };
                    put(&mut out, &mut seen, TestResult { name: name.to_string(), outcome, ms: secs_ms(secs), error: None });
                }
            }
        }
    }
    for r in out.iter_mut().filter(|r| r.outcome == "failed") {
        r.error = errors.get(&r.name).and_then(|e| clip(e));
    }
    Parsed { runner: if nextest { "nextest" } else { "cargo" }.into(), tests: out, total_ms: any_total.then_some(total), counts }
}

const PY_STATUS: [&str; 6] = ["PASSED", "FAILED", "ERROR", "SKIPPED", "XFAIL", "XPASS"];

fn py_outcome(s: &str) -> &'static str {
    match s {
        "PASSED" | "XPASS" => "passed",
        "FAILED" | "ERROR" => "failed",
        _ => "skipped",
    }
}

/// pytest's `-v` lines (`tests/a.py::t PASSED [ 10%]`), its short summary
/// (`FAILED tests/a.py::t - Assert…`), `--durations` lines and failure
/// sections.
fn parse_pytest(text: &str) -> Parsed {
    let mut out = Vec::new();
    let mut seen = HashMap::new();
    let mut durations: HashMap<String, i64> = HashMap::new();
    let mut total = None;
    let mut counts = None;
    let mut sections: HashMap<String, Vec<String>> = HashMap::new();
    let mut in_sec: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        let t = line.trim();
        // A failure's section: "____ test_name ____" (or "____ Class.test ____").
        if t.starts_with("___") && t.ends_with("___") && t.len() > 6 {
            let name = t.trim_matches('_').trim().to_string();
            in_sec = (!name.is_empty() && !name.contains(' ')).then_some(name);
            continue;
        }
        if let Some((c, ms)) = pytest_summary_in(t) {
            in_sec = None;
            counts = Some(c);
            total = ms.or(total);
            continue;
        }
        if t.starts_with("===") {
            in_sec = None;
            continue;
        }
        if let Some(name) = &in_sec {
            let e = sections.entry(name.clone()).or_default();
            if e.len() < 60 {
                e.push(line.to_string());
            }
            continue;
        }
        let words: Vec<&str> = t.split_whitespace().collect();
        // --durations: "0.52s call     tests/a.py::t"
        if words.len() >= 3 && words[1] == "call" && words[2].contains("::") {
            if let Some(ms) = secs_ms(words[0]) {
                durations.insert(words[2].to_string(), ms);
            }
            continue;
        }
        for (i, w) in words.iter().enumerate() {
            if !PY_STATUS.contains(w) {
                continue;
            }
            // "STATUS nodeid" (summary, xdist) or "nodeid STATUS" (-v).
            let node = words.get(i + 1).filter(|n| n.contains("::")).or_else(|| i.checked_sub(1).and_then(|j| words.get(j)).filter(|n| n.contains("::")));
            if let Some(node) = node {
                put(&mut out, &mut seen, TestResult { name: node.to_string(), outcome: py_outcome(w), ms: None, error: None });
                break;
            }
        }
    }
    for r in out.iter_mut() {
        r.ms = durations.get(&r.name).copied();
        if r.outcome == "failed" {
            let short = r.name.rsplit("::").next().unwrap_or("");
            let class_form = r.name.split("::").skip(1).collect::<Vec<_>>().join(".");
            let sec = sections.get(short).or_else(|| sections.get(&class_form));
            if let Some(lines) = sec {
                // The lines that say what went wrong ("E   assert …"), else the section's end.
                let es: Vec<String> = lines.iter().filter(|l| l.trim_start().starts_with("E ")).cloned().collect();
                r.error = clip(if es.is_empty() { &lines[lines.len().saturating_sub(12)..] } else { &es });
            }
        }
    }
    Parsed { runner: "pytest".into(), tests: out, total_ms: total, counts }
}

fn marker(s: &str) -> Option<(&'static str, &str)> {
    for (m, o) in [("✓", "passed"), ("✔", "passed"), ("√", "passed"), ("✕", "failed"), ("✗", "failed"), ("×", "failed"), ("✖", "failed"), ("○", "skipped"), ("↓", "skipped")] {
        if let Some(rest) = s.strip_prefix(m) {
            return Some((o, rest.trim_start()));
        }
    }
    None
}

/// A trailing time: "(5 ms)", "5ms", "(1.2 s)".
fn trailing_time(s: &str) -> (String, Option<i64>) {
    let s = s.trim_end();
    if s.ends_with(')') {
        if let Some(open) = s.rfind('(') {
            if let Some(ms) = secs_ms(&s[open..].replace(' ', "")) {
                return (s[..open].trim_end().to_string(), Some(ms));
            }
        }
    }
    if let Some((head, last)) = s.rsplit_once(' ') {
        if last.ends_with("ms") || (last.ends_with('s') && last[..last.len() - 1].parse::<f64>().is_ok()) {
            if let Some(ms) = secs_ms(last) {
                return (head.trim_end().to_string(), Some(ms));
            }
        }
    }
    (s.to_string(), None)
}

/// jest (`PASS file`, indented describes, `✓ name (5 ms)`, `● a › b`
/// failures) and vitest (`✓ file > suite > name 5ms`, `FAIL file > …`).
fn parse_js(text: &str) -> Parsed {
    let mut out = Vec::new();
    let mut seen = HashMap::new();
    let mut file: Option<String> = None;
    let mut describes: Vec<(usize, String)> = Vec::new();
    let mut vitest = false;
    let mut counts = None;
    let mut errors: HashMap<String, Vec<String>> = HashMap::new();
    let mut in_err: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        let t = line.trim_start();
        let indent = line.len() - t.len();
        // jest's "Tests:       1 failed, 3 passed, 4 total"; vitest's
        // "Tests  1 failed | 3 passed (4)".
        if let Some(rest) = t.strip_prefix("Tests:").or_else(|| t.strip_prefix("Tests ")) {
            if let Some(c) = counts_in(rest) {
                counts = Some(c);
                vitest |= !t.starts_with("Tests:");
                continue;
            }
        }
        if let Some(rest) = t.strip_prefix("● ") {
            let name = rest.trim().to_string();
            in_err = (!name.contains("Test suite failed")).then_some(name);
            continue;
        }
        if let Some(name) = &in_err {
            if t.starts_with("PASS ") || t.starts_with("FAIL ") || t.starts_with("Test Suites:") {
                in_err = None;
            } else {
                let e = errors.entry(name.clone()).or_default();
                if e.len() < 30 && !t.is_empty() {
                    e.push(t.to_string());
                }
                continue;
            }
        }
        for (head, failed) in [("PASS ", false), ("FAIL ", true)] {
            if let Some(rest) = t.strip_prefix(head) {
                let rest = rest.trim();
                if rest.contains(" > ") {
                    // vitest's failure summary: "FAIL  file > suite > name"
                    vitest = true;
                    if failed {
                        let name = rest.split(" > ").map(str::trim).collect::<Vec<_>>().join(" › ");
                        let (name, _) = trailing_time(&name);
                        put(&mut out, &mut seen, TestResult { name, outcome: "failed", ms: None, error: None });
                    }
                } else {
                    file = Some(rest.split_whitespace().next().unwrap_or("").to_string());
                    describes.clear();
                }
            }
        }
        let Some((outcome, rest)) = marker(t) else {
            // A describe block's title: an indented line under a file, no marker.
            if file.is_some() && indent >= 2 && !t.is_empty() && !t.starts_with("at ") && !t.contains(':') && t.len() < 200 {
                while describes.last().is_some_and(|(i, _)| *i >= indent) {
                    describes.pop();
                }
                describes.push((indent, t.to_string()));
            }
            continue;
        };
        let rest = rest.strip_prefix("skipped ").or_else(|| rest.strip_prefix("todo ")).unwrap_or(rest);
        let (name, ms) = trailing_time(rest);
        if name.contains(" > ") || name.contains(".test.") || name.contains(".spec.") {
            // vitest: "file > suite > name"; a file's own line "file (3 tests)" is not a test.
            if name.ends_with("tests)") || name.ends_with("test)") || !name.contains(" > ") {
                continue;
            }
            vitest = true;
            let name = name.split(" > ").map(str::trim).collect::<Vec<_>>().join(" › ");
            put(&mut out, &mut seen, TestResult { name, outcome, ms, error: None });
            continue;
        }
        while describes.last().is_some_and(|(i, _)| *i >= indent) {
            describes.pop();
        }
        let mut parts: Vec<String> = file.iter().cloned().collect();
        parts.extend(describes.iter().map(|(_, d)| d.clone()));
        parts.push(name);
        put(&mut out, &mut seen, TestResult { name: parts.join(" › "), outcome, ms, error: None });
    }
    for r in out.iter_mut().filter(|r| r.outcome == "failed") {
        // jest names a failure "describe › test" without its file.
        let key = errors.keys().find(|k| r.name.ends_with(k.as_str())).cloned();
        r.error = key.and_then(|k| clip(&errors[&k]));
    }
    Parsed { runner: if vitest { "vitest" } else { "jest" }.into(), tests: out, total_ms: None, counts }
}

/// go test -v: `--- PASS: TestX (0.00s)`, the package from `ok  pkg 0.1s`
/// or `FAIL pkg 0.2s`, and a failure's `x_test.go:12:` lines.
fn parse_go(text: &str) -> Parsed {
    let mut out: Vec<TestResult> = Vec::new();
    let mut pending: Vec<usize> = Vec::new();
    let mut logs: HashMap<String, Vec<String>> = HashMap::new();
    let mut current: Option<String> = None;
    let mut total = 0i64;
    let mut any = false;
    for raw in text.lines() {
        let t = raw.trim();
        if let Some(name) = t.strip_prefix("=== RUN") {
            current = Some(name.trim().to_string());
            continue;
        }
        if let Some(rest) = t.strip_prefix("--- ") {
            let (status, rest) = rest.split_once(':').unwrap_or(("", ""));
            let outcome = match status {
                "PASS" => "passed",
                "FAIL" => "failed",
                "SKIP" => "skipped",
                _ => continue,
            };
            let (name, ms) = trailing_time(rest.trim());
            let error = if outcome == "failed" { logs.get(&name).and_then(|l| clip(l)) } else { None };
            pending.push(out.len());
            out.push(TestResult { name, outcome, ms, error });
            continue;
        }
        let words: Vec<&str> = t.split_whitespace().collect();
        if matches!(words.first(), Some(&"ok") | Some(&"FAIL")) && words.len() >= 2 && !words[1].starts_with('-') {
            let pkg = words[1];
            if let Some(ms) = words.get(2).and_then(|w| secs_ms(w)) {
                total += ms;
                any = true;
            }
            for i in pending.drain(..) {
                out[i].name = format!("{pkg} › {}", out[i].name);
            }
            continue;
        }
        if let Some(c) = &current {
            if raw.starts_with("    ") && t.contains("_test.go:") {
                let l = logs.entry(c.split('/').next().unwrap_or(c).to_string()).or_default();
                if l.len() < 20 {
                    l.push(t.to_string());
                }
                logs.entry(c.clone()).or_default().push(t.to_string());
            }
        }
    }
    Parsed { runner: "go".into(), tests: out, total_ms: any.then_some(total), counts: None }
}

/// Parses a test command's output with the parser its command names first,
/// then the others; `runner: "other"` and no tests when none finds any.
pub fn parse(command: &str, output: &str) -> Parsed {
    let c = command.to_ascii_lowercase();
    let mut order: Vec<fn(&str) -> Parsed> = Vec::new();
    if c.contains("cargo") {
        order.push(parse_cargo);
    }
    if c.contains("pytest") || c.contains("tox") {
        order.push(parse_pytest);
    }
    if c.contains("go test") {
        order.push(parse_go);
    }
    if c.contains("jest") || c.contains("vitest") || c.contains("npm") || c.contains("yarn") || c.contains("pnpm") || c.contains("bun") || c.contains("npx") {
        order.push(parse_js);
    }
    for f in [parse_cargo as fn(&str) -> Parsed, parse_pytest, parse_go, parse_js] {
        if !order.iter().any(|g| *g as usize == f as usize) {
            order.push(f);
        }
    }
    let mut quiet: Option<Parsed> = None;
    for f in order {
        let mut p = f(output);
        if !p.tests.is_empty() {
            p.tests.truncate(MAX_TESTS);
            return p;
        }
        if quiet.is_none() && p.counts.is_some() {
            quiet = Some(p);
        }
    }
    // Only a summary line: its counts, and no test made up.
    quiet.unwrap_or(Parsed { runner: "other".into(), tests: Vec::new(), total_ms: None, counts: None })
}

// ------------------------------------------------- shell words, invocations

/// One simple command of a shell line: the environment it sets before
/// its program (`PYTHONPATH=. pytest`) and its words, quotes removed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Simple {
    pub env: Vec<String>,
    pub words: Vec<String>,
    /// A word had `$`, a backtick or a glob in it: not a plain command.
    pub expands: bool,
}

/// The words a shell puts before a program without being it.
const SHELL_LEAD: [&str; 15] = ["do", "then", "else", "elif", "time", "!", "{", "}", "exec", "command", "nohup", "env", "nice", "builtin", "done"];

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit()))
}

/// A shell line's simple commands, split at `;`, `&&`, `||`, `|`, `&`,
/// parentheses and new lines outside quotes, without redirections
/// (`> log 2>&1`), the shell's own words (`do`, `time`, `!`) and the
/// assignments before each program (kept apart as its `env`). Command
/// substitutions stay inside their word, which is then marked `expands`.
pub fn simple_commands(line: &str) -> Vec<Simple> {
    let mut out: Vec<Simple> = Vec::new();
    let mut cur = Simple::default();
    let mut word = String::new();
    let mut in_word = false;
    let mut skip_next = false;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let end_word = |word: &mut String, in_word: &mut bool, cur: &mut Simple, skip: &mut bool| {
        if *in_word {
            if *skip {
                *skip = false;
            } else {
                cur.words.push(std::mem::take(word));
            }
        }
        word.clear();
        *in_word = false;
    };
    let finish = |cur: &mut Simple, out: &mut Vec<Simple>| {
        let mut c = std::mem::take(cur);
        let mut lead = 0;
        while lead < c.words.len() && (SHELL_LEAD.contains(&c.words[lead].as_str()) || is_assignment(&c.words[lead])) {
            if is_assignment(&c.words[lead]) {
                c.env.push(c.words[lead].clone());
            }
            lead += 1;
        }
        c.words.drain(..lead);
        if !c.words.is_empty() {
            out.push(c);
        }
    };
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    if chars[i] == '$' || chars[i] == '`' {
                        cur.expands = true;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '\\' if i + 1 < chars.len() => {
                in_word = true;
                if chars[i + 1] != '\n' {
                    word.push(chars[i + 1]);
                }
                i += 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                // $( … ): kept whole in its word.
                in_word = true;
                cur.expands = true;
                let mut depth = 0;
                while i < chars.len() {
                    word.push(chars[i]);
                    if chars[i] == '(' {
                        depth += 1;
                    } else if chars[i] == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
            }
            '`' => {
                in_word = true;
                cur.expands = true;
                word.push(ch);
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    word.push(chars[i]);
                    i += 1;
                }
                word.push('`');
            }
            '>' | '<' => {
                // A redirection: its fd ("2>"), its operator and its target go.
                if in_word && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                end_word(&mut word, &mut in_word, &mut cur, &mut skip_next);
                while i + 1 < chars.len() && matches!(chars[i + 1], '>' | '<' | '&' | '|') {
                    i += 1;
                }
                // "2>&1": the target is a descriptor, part of the operator.
                if chars.get(i).is_some_and(|c| *c == '&') && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit() || *c == '-') {
                    i += 1;
                } else {
                    skip_next = true;
                }
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                end_word(&mut word, &mut in_word, &mut cur, &mut skip_next);
                finish(&mut cur, &mut out);
            }
            c if c.is_whitespace() => end_word(&mut word, &mut in_word, &mut cur, &mut skip_next),
            c => {
                if matches!(c, '$' | '*' | '?') {
                    cur.expands = true;
                }
                in_word = true;
                word.push(c);
            }
        }
        i += 1;
    }
    end_word(&mut word, &mut in_word, &mut cur, &mut skip_next);
    finish(&mut cur, &mut out);
    out
}

/// Whether a simple command runs pytest: (its index of `pytest`).
fn pytest_at(words: &[String]) -> Option<usize> {
    words.iter().position(|x| x == "pytest" || x == "py.test" || x.ends_with("/pytest") || x.ends_with("/py.test"))
}

/// The environment a command sets for its test runner (`FOO=/nonexistent
/// pytest`), sorted: runs with some are not the code's normal result
/// (a check of a skip, a broken dependency on purpose), and are not
/// compared with runs without.
pub fn variant(command: &str) -> String {
    let cmds = simple_commands(command);
    let runner = cmds.iter().find(|c| crate::flow::is_test(&c.words.join(" ")) || pytest_at(&c.words).is_some());
    let mut env: Vec<String> = runner.map(|c| c.env.clone()).unwrap_or_default();
    env.sort();
    env.join(" ")
}

/// A call's output cut into one piece per run of its test runner: a
/// loop (`for i in 1 2 3; do pytest; done`) or a list of runs prints
/// pytest's summary line, cargo's `Finished` line or jest's `Tests:` line
/// once per run. One piece when it ran once (or the runner is not known).
pub fn invocations(command: &str, output: &str) -> Vec<String> {
    let c = command.to_ascii_lowercase();
    let lines: Vec<&str> = output.lines().collect();
    let one = || vec![output.to_string()];
    let join = |ls: &[&str]| ls.join("\n");
    if c.contains("pytest") || c.contains("py.test") || c.contains("tox") {
        let ends: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| pytest_summary_in(l).is_some()).map(|(i, _)| i).collect();
        if ends.len() < 2 {
            return one();
        }
        let mut out = Vec::new();
        let mut from = 0;
        for (k, &e) in ends.iter().enumerate() {
            let to = if k + 1 == ends.len() { lines.len() } else { e + 1 };
            out.push(join(&lines[from..to]));
            from = e + 1;
        }
        return out;
    }
    let starts: Vec<usize> = if c.contains("cargo") {
        lines.iter().enumerate().filter(|(_, l)| l.trim_start().starts_with("Finished ") && l.contains(" in ")).map(|(i, _)| i).collect()
    } else {
        Vec::new()
    };
    if starts.len() >= 2 {
        let mut out = Vec::new();
        for (k, &s) in starts.iter().enumerate() {
            let from = if k == 0 { 0 } else { s };
            let to = starts.get(k + 1).copied().unwrap_or(lines.len());
            out.push(join(&lines[from..to]));
        }
        return out;
    }
    if c.contains("jest") || c.contains("vitest") || c.contains("npm") || c.contains("npx") || c.contains("yarn") || c.contains("pnpm") || c.contains("bun") {
        let ends: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim_start();
                (t.starts_with("Tests:") || t.starts_with("Tests ")) && (t.contains("passed") || t.contains("failed"))
            })
            .map(|(i, _)| i)
            .collect();
        if ends.len() >= 2 {
            let mut out = Vec::new();
            let mut from = 0;
            for (k, &e) in ends.iter().enumerate() {
                let to = if k + 1 == ends.len() { lines.len() } else { e + 1 };
                out.push(join(&lines[from..to]));
                from = e + 1;
            }
            return out;
        }
    }
    one()
}

// ------------------------------------------------------------ junit xml

/// `&lt;` and its kind back to text.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The attributes of a tag's text (`name="x" time="0.1"`).
fn attrs(tag: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = tag;
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].split_whitespace().last().unwrap_or("").to_string();
        let after = rest[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
        let Some(close) = after[1..].find(q) else { break };
        out.insert(key, unescape(&after[1..1 + close]));
        rest = &after[close + 2..];
    }
    out
}

/// A junit test's id as pytest prints it: `tests/test_a.py::Class::test`.
/// xunit1 has the file; xunit2 only a dotted class name, read against the
/// checkout (the longest prefix that is a file), else up to its first
/// capitalised part.
fn junit_name(a: &HashMap<String, String>, dir: Option<&Path>) -> String {
    let name = a.get("name").cloned().unwrap_or_default();
    let class = a.get("classname").cloned().unwrap_or_default();
    if class.is_empty() {
        return name;
    }
    let parts: Vec<&str> = class.split('.').collect();
    let module_len = if let Some(file) = a.get("file").filter(|f| f.ends_with(".py")) {
        let m = file.trim_end_matches(".py").replace(['/', '\\'], ".");
        if class == m || class.starts_with(&format!("{m}.")) {
            Some(m.split('.').count())
        } else {
            None
        }
    } else {
        None
    };
    let module_len = module_len
        .or_else(|| dir.and_then(|d| (1..=parts.len()).rev().find(|k| d.join(format!("{}.py", parts[..*k].join("/"))).is_file())))
        .unwrap_or_else(|| parts.iter().position(|p| p.starts_with(|c: char| c.is_ascii_uppercase())).unwrap_or(parts.len()).max(1));
    let file = format!("{}.py", parts[..module_len].join("/"));
    let mut id = vec![file];
    id.extend(parts[module_len..].iter().map(|p| p.to_string()));
    id.push(name);
    id.join("::")
}

/// A junit xml report (pytest's `--junitxml`, and any runner's): its tests
/// and the suites' time.
pub fn parse_junit(xml: &str, dir: Option<&Path>) -> Parsed {
    let mut out = Vec::new();
    let mut seen = HashMap::new();
    let mut total: Option<i64> = None;
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        if let Some(t) = tag.strip_prefix("testsuite ").filter(|_| !tag.starts_with("testsuites")) {
            if let Some(ms) = attrs(t).get("time").and_then(|s| s.parse::<f64>().ok()) {
                *total.get_or_insert(0) += (ms * 1000.0).round() as i64;
            }
            continue;
        }
        let Some(t) = tag.strip_prefix("testcase") else { continue };
        let a = attrs(t);
        let body = if tag.ends_with('/') {
            ""
        } else {
            let b = &rest[end + 1..];
            &b[..b.find("</testcase>").unwrap_or(b.len())]
        };
        let (outcome, kind) = if body.contains("<failure") {
            ("failed", "failure")
        } else if body.contains("<error") {
            ("failed", "error")
        } else if body.contains("<skipped") {
            ("skipped", "skipped")
        } else {
            ("passed", "")
        };
        let error = (outcome == "failed").then(|| {
            let at = body.find(&format!("<{kind}")).unwrap_or(0);
            let el = &body[at..];
            let open_end = el.find('>').unwrap_or(el.len());
            let msg = attrs(&el[..open_end]).get("message").cloned().unwrap_or_default();
            let text = el.get(open_end + 1..).map(|x| unescape(&x[..x.find("</").unwrap_or(x.len())])).unwrap_or_default();
            clip(&[msg, text].iter().filter(|x| !x.trim().is_empty()).cloned().collect::<Vec<_>>())
        });
        let ms = a.get("time").and_then(|s| s.parse::<f64>().ok()).map(|x| (x * 1000.0).round() as i64);
        put(&mut out, &mut seen, TestResult { name: junit_name(&a, dir), outcome, ms, error: error.flatten() });
        if out.len() >= MAX_TESTS {
            break;
        }
    }
    let counts = (!out.is_empty()).then(|| Counts::of(&out));
    Parsed { runner: "junit".into(), tests: out, total_ms: total, counts }
}

/// Where a session's pytest writes its report (see [`session_env`]).
pub fn junit_path(session_dir: &Path) -> std::path::PathBuf {
    session_dir.join("pytest-junit.xml")
}

/// What Workbench adds to a session's environment so that every pytest it
/// runs writes a junit report, test by test, whatever its console says
/// (`-q`, a pipe to `tail`): `PYTEST_ADDOPTS` with `--junitxml` into the
/// session's own folder (after the user's own `PYTEST_ADDOPTS`). Not when
/// the repository's pytest config sets a junit report of its own, which
/// this would move.
pub fn session_env(cwd: &Path, session_dir: &Path) -> Vec<(String, String)> {
    for f in ["pytest.ini", "pyproject.toml", "setup.cfg", "tox.ini"] {
        if std::fs::read_to_string(cwd.join(f)).is_ok_and(|t| t.contains("junit")) {
            return Vec::new();
        }
    }
    let path = junit_path(session_dir);
    let p = path.to_string_lossy();
    if p.contains('\'') {
        return Vec::new();
    }
    let own = std::env::var("PYTEST_ADDOPTS").unwrap_or_default();
    let add = format!("--junitxml='{p}' -o junit_family=xunit1");
    vec![("PYTEST_ADDOPTS".into(), if own.trim().is_empty() { add } else { format!("{own} {add}") })]
}

/// A junit report a call named (`--junitxml=x.xml`, `--junit-xml x.xml`),
/// relative to the checkout.
fn junit_named(command: &str, dir: &Path) -> Option<std::path::PathBuf> {
    let words: Vec<&str> = command.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        for flag in ["--junitxml", "--junit-xml"] {
            let v = if let Some(v) = w.strip_prefix(&format!("{flag}=")) {
                Some(v)
            } else if *w == flag {
                words.get(i + 1).copied()
            } else {
                None
            };
            if let Some(v) = v {
                let v = v.trim_matches(|c| c == '"' || c == '\'');
                let p = Path::new(v);
                return Some(if p.is_absolute() { p.to_path_buf() } else { dir.join(p) });
            }
        }
    }
    None
}

// ---------------------------------------------------------------- capture

/// The one call the server makes after each hook was recorded: test runs
/// from Bash calls, file edits and transcript paths for the alerts. Never
/// fails and never waits on git or a transcript: those run on a thread.
pub fn after_hook(sup: &Arc<Supervisor>, id: &str, hook: &Json) {
    let s = |k: &str| hook.get(k).and_then(Json::as_str).unwrap_or("");
    let kind = s("hook_event_name");
    let tool = s("tool_name");
    if !s("transcript_path").is_empty() {
        crate::alerts::note_transcript(id, s("transcript_path"));
    }
    match (kind, tool) {
        ("PostToolUse" | "PostToolUseFailure", "Bash") => {
            let cmd = hook["tool_input"]["command"].as_str().unwrap_or("");
            if is_test_command(cmd) {
                let (sup, id, hook) = (sup.clone(), id.to_string(), hook.clone());
                let _ = std::thread::Builder::new().name(format!("tests-{id}")).spawn(move || record_from_hook(&sup, &id, &hook));
            }
        }
        ("PostToolUse", "Edit" | "Write" | "MultiEdit" | "NotebookEdit") => crate::alerts::record_edit(sup, id, hook),
        ("Stop", _) => {
            let (sup, id, path) = (sup.clone(), id.to_string(), s("transcript_path").to_string());
            let _ = std::thread::Builder::new().name(format!("tests-stop-{id}")).spawn(move || recover_turn(&sup, &id, &path));
        }
        _ => {}
    }
}

/// Whether a command runs tests: a runner Flow knows, or a virtualenv's
/// pytest (`.venv/bin/pytest`, `.venv/bin/python -m pytest`, `./pytest`).
pub fn is_test_command(cmd: &str) -> bool {
    crate::flow::is_test(cmd) || cmd.split_whitespace().collect::<Vec<_>>().windows(2).any(|w| w[0] == "-m" && w[1] == "pytest") || cmd.split_whitespace().any(|w| w.ends_with("/pytest") || w.ends_with("/py.test"))
}

/// The text of a tool's response or a transcript's `tool_result` content.
fn text_of(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Array(a) => a.iter().map(|c| c.get("text").and_then(Json::as_str).map(str::to_string).unwrap_or_else(|| text_of(c))).collect::<Vec<_>>().join("\n"),
        Json::Object(o) => {
            let mut parts = Vec::new();
            for k in ["stdout", "stderr", "output", "error", "content"] {
                if let Some(x) = o.get(k) {
                    let t = text_of(x);
                    if !t.is_empty() {
                        parts.push(t);
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// "Exit code 101" in a failure's text, else 1.
fn exit_code_in(text: &str) -> i64 {
    text.lines().find_map(|l| l.trim().strip_prefix("Exit code ").and_then(|n| n.trim().parse().ok())).unwrap_or(1)
}

fn hash8(s: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// One Bash call that ran tests, as found.
struct Call {
    command: String,
    output: String,
    exit_code: i64,
    use_id: Option<String>,
    end_ms: i64,
}

fn record_from_hook(sup: &Supervisor, id: &str, hook: &Json) {
    let r = &hook["tool_response"];
    if r.get("interrupted").and_then(Json::as_bool) == Some(true) {
        return;
    }
    let failed = hook["hook_event_name"] == "PostToolUseFailure" || r.get("is_error").and_then(Json::as_bool) == Some(true);
    let mut output = text_of(r);
    if let Some(e) = hook.get("error").and_then(Json::as_str) {
        output = format!("{output}\n{e}");
    }
    let call = Call {
        command: hook["tool_input"]["command"].as_str().unwrap_or("").to_string(),
        exit_code: if failed { exit_code_in(&output) } else { 0 },
        output,
        use_id: hook["tool_use_id"].as_str().map(str::to_string),
        end_ms: now_ms(),
    };
    record(sup, id, call);
}

/// The agent's recent events, newest first: (seq, ts_ms, kind, data).
fn recent_events(sup: &Supervisor, agent: &str, n: usize) -> Vec<(i64, i64, String, Json)> {
    let p = Params::new().set("agent", Value::Str(agent.to_string()));
    let rows = sup.db().query(&format!("SELECT seq, ts, kind, data FROM events WHERE agent = :agent ORDER BY seq DESC LIMIT {n}"), &p).unwrap_or_default();
    rows.iter()
        .map(|r| (get_int(&r[0]).unwrap_or(0), get_ms(&r[1]).unwrap_or(0), get_str(&r[2]).unwrap_or_default(), get_str(&r[3]).and_then(|d| serde_json::from_str(&d).ok()).unwrap_or(Json::Null)))
        .collect()
}

/// The `PreToolUse` of a call: by `tool_use_id`, else the newest of its
/// command; (seq, ts_ms).
fn pre_of(evs: &[(i64, i64, String, Json)], use_id: Option<&str>, command: &str) -> Option<(i64, i64)> {
    let pres = || evs.iter().filter(|e| e.2 == "PreToolUse" && e.3["tool_name"] == "Bash");
    use_id
        .and_then(|u| pres().find(|e| e.3["tool_use_id"].as_str() == Some(u)))
        .or_else(|| pres().find(|e| e.3["tool_input"]["command"].as_str() == Some(command)))
        .map(|e| (e.0, e.1))
}

/// The commit of a checkout and a hash of its uncommitted changes
/// (`clean` without any): together, "the same code".
fn code_of(dir: &Path) -> (Option<String>, String) {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let commit = git(&["rev-parse", "HEAD"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let diff = git(&["diff", "HEAD", "--no-ext-diff", "--no-color"]).unwrap_or_default();
    let untracked = git(&["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    let tree = if diff.trim().is_empty() && untracked.trim().is_empty() { "clean".to_string() } else { hash8(&format!("{diff}\u{0}{untracked}")) };
    (commit, tree)
}

/// The time a call waited on a permission prompt between `start` and
/// `end` (ms): its questions' time open when each was answered in
/// Workbench, `None` when one was answered in the terminal or not at all
/// (then when the run itself began is not known). PreToolUse fires before
/// the prompt, PostToolUse after the run: the wait is inside that span.
fn permission_wait(sup: &Supervisor, agent: &str, start: i64, end: i64) -> Option<i64> {
    let p = Params::new().set("agent", Value::Str(agent.to_string())).set("from", ts(start)).set("to", ts(end));
    let rows = sup.db().query("SELECT answered_by, created, answered FROM questions WHERE agent = :agent AND kind = 'permission' AND created >= :from AND created <= :to LIMIT 100", &p).unwrap_or_default();
    let mut wait = 0;
    for r in rows {
        let by = get_str(&r[0]).unwrap_or_default();
        let (Some(c), Some(a)) = (get_ms(&r[1]), get_ms(&r[2])) else { return None };
        if by == "terminal" || a > end {
            return None;
        }
        wait += (a - c).max(0);
    }
    Some(wait)
}

/// A junit report written during a call (after its PreToolUse, by `end + 5 s`).
fn fresh_junit(path: &Path, start: Option<i64>, end: i64, dir: &Path, use_id: Option<&str>) -> Option<Parsed> {
    let md = std::fs::metadata(path).ok()?;
    if md.len() > 64 << 20 {
        return None;
    }
    let at = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    let mt = at.as_millis() as i64;
    if mt < start.map_or(end - 120_000, |s| s - 50) || mt > end + 5000 {
        return None;
    }
    // A report is one call's: a call that starts right after another
    // (within the slack above) must not read the report the first wrote.
    let key = (path.to_path_buf(), at.as_nanos(), md.len());
    let mut read = junits_read().lock().unwrap();
    if read.get(&key).is_some_and(|u| use_id.is_none() || u.as_deref() != use_id) {
        return None;
    }
    let p = parse_junit(&std::fs::read_to_string(path).ok()?, Some(dir));
    if p.tests.is_empty() {
        return None;
    }
    if read.len() > 4096 {
        read.clear();
    }
    read.insert(key, use_id.map(str::to_string));
    Some(p)
}

/// The junit reports already read, by file, time and size, with the call
/// that read each (the same call may be recorded again).
fn junits_read() -> &'static std::sync::Mutex<HashMap<(PathBuf, u128, u64), Option<String>>> {
    static R: std::sync::OnceLock<std::sync::Mutex<HashMap<(PathBuf, u128, u64), Option<String>>>> = std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// One run of a test runner within a call, as it will be written.
#[derive(Debug, Default)]
struct Piece {
    runner: String,
    tests: Vec<TestResult>,
    total_ms: Option<i64>,
    counts: Option<Counts>,
    source: &'static str,
    exit_code: i64,
}

/// What a call's output (and, for its last run, a junit report) says of
/// each run of its runner. A loop that runs pytest five times is five
/// pieces; a run that printed only its counts gets its tests by name
/// when the same call named every test of another run with the same
/// total and its counts leave no doubt (no skips, each failure named).
fn pieces_of(command: &str, output: &str, exit_code: i64, junit: Option<Parsed>) -> Vec<Piece> {
    let segs = invocations(command, output);
    let n = segs.len();
    let mut out: Vec<Piece> = Vec::new();
    let mut junit = junit;
    for (k, seg) in segs.iter().enumerate() {
        let console = parse(command, seg);
        // The session's junit report is the last run's: each run writes it again.
        let j = if k + 1 == n { junit.take() } else { None };
        let j = j.filter(|j| n == 1 || console.counts.is_none() || console.counts == j.counts);
        let code = if n == 1 { exit_code } else { console.counts.map_or(exit_code, |c| i64::from(c.failed > 0)) };
        let piece = match j {
            Some(j) => {
                // The console's errors are fuller than the report's messages.
                let mut tests = j.tests;
                for t in tests.iter_mut().filter(|t| t.outcome == "failed") {
                    if let Some(c) = console.tests.iter().find(|c| c.name == t.name).and_then(|c| c.error.clone()) {
                        t.error = Some(c);
                    }
                }
                let runner = if console.runner != "other" && console.runner != "junit" { console.runner.clone() } else if command.contains("pytest") || command.contains("py.test") { "pytest".into() } else { "junit".into() };
                Piece { runner, tests, total_ms: j.total_ms.or(console.total_ms), counts: j.counts, source: "junit", exit_code: code }
            }
            None => {
                let source = if !console.tests.is_empty() { "console" } else if console.counts.is_some() { "summary" } else { "exit" };
                let counts = console.counts.or_else(|| (!console.tests.is_empty()).then(|| Counts::of(&console.tests)));
                Piece { runner: console.runner, tests: console.tests, total_ms: console.total_ms, counts, source, exit_code: code }
            }
        };
        out.push(piece);
    }
    if out.len() > 1 {
        // The tests of a run that named them all, for the runs that only counted.
        let known: Option<Vec<String>> = out.iter().find(|p| !p.tests.is_empty() && p.counts.is_some_and(|c| c.total() == p.tests.len() as i64)).map(|p| p.tests.iter().map(|t| t.name.clone()).collect());
        if let Some(known) = known {
            for p in out.iter_mut() {
                let Some(c) = p.counts else { continue };
                let named_failed: Vec<&TestResult> = p.tests.iter().filter(|t| t.outcome == "failed").collect();
                let sure = c.skipped == 0 && c.total() == known.len() as i64 && named_failed.len() as i64 == c.failed && p.tests.iter().all(|t| known.contains(&t.name)) && p.tests.len() < known.len();
                if !sure {
                    continue;
                }
                for name in &known {
                    if !p.tests.iter().any(|t| &t.name == name) {
                        p.tests.push(TestResult { name: name.clone(), outcome: "passed", ms: None, error: None });
                    }
                }
                p.source = "inferred";
            }
        }
    }
    out
}

/// Where a call ran: its repository and checkout.
struct Place {
    repo: String,
    cwd: String,
}

/// Writes a call's tests as rows of `test_runs`, one per test it names
/// (a junit report it wrote first, else its output), and each run of its
/// runner as a row of `test_summaries`: its counts, exit code, time and
/// how much of that it waited for a permission. A call whose output names
/// no test is only its summary: no test is made up from its command. A
/// call that ran its runner several times (a loop) is that many runs, one
/// after another. A call already recorded (its run ids are made from its
/// `tool_use_id`) is replaced.
fn record(sup: &Supervisor, agent: &str, call: Call) {
    let Some(a) = sup.agent_row(agent) else { return };
    record_at(sup, agent, &Place { repo: a.repo, cwd: a.cwd }, call);
}

fn record_at(sup: &Supervisor, agent: &str, at: &Place, call: Call) {
    let evs = if agent.is_empty() { Vec::new() } else { recent_events(sup, agent, 400) };
    let mine = by_workbench(&call.use_id);
    let pre = if mine { None } else { pre_of(&evs, call.use_id.as_deref(), &call.command) };
    let seq = if mine { None } else { pre.map(|p| p.0).or_else(|| evs.first().map(|e| e.0)) };
    let start = pre.map(|p| p.1);
    let base = match &call.use_id {
        Some(u) => format!("t{}", hash8(&format!("{agent}:{u}"))),
        None => crate::levels::new_id("t"),
    };
    let dir = Path::new(&at.cwd);
    let junit = if mine || agent.is_empty() { None } else { [junit_named(&call.command, dir), Some(junit_path(&sup.session_dir(agent)))].into_iter().flatten().find_map(|p| fresh_junit(&p, start, call.end_ms, dir, call.use_id.as_deref())) };
    let pieces = pieces_of(&call.command, &call.output, call.exit_code, junit);
    let n = pieces.len();
    // The run's time: the runner's own, else (one run) the call's span
    // less the time it waited for a permission; unknown when that wait is.
    let wait = start.map(|s| permission_wait(sup, agent, s, call.end_ms));
    let wait_ms = wait.flatten();
    let (commit, tree) = code_of(dir);
    let now = now_ms();
    let end = call.end_ms.min(now);
    let command = one_line(&call.command, 2000);
    for (k, piece) in pieces.into_iter().enumerate() {
        let run = if k == 0 { base.clone() } else { format!("t{}", hash8(&format!("{agent}:{}:{k}", call.use_id.as_deref().unwrap_or(&base)))) };
        // One after another, within the call's last milliseconds.
        let at_ms = end - (n - 1 - k) as i64;
        let run_ms = piece.total_ms.or_else(|| match (n, start, wait) {
            (1, Some(s), Some(Some(w))) => Some((call.end_ms - s - w).max(0)),
            _ => None,
        });
        let piece_wait = if k == 0 { wait_ms } else { None };
        let tests = &piece.tests;
        let counts = piece.counts;
        let complete = !tests.is_empty() && counts.is_some_and(|c| c.total() == tests.len() as i64);
        let sp = Params::new()
            .set("id", Value::Str(run.clone()))
            .set("repo", Value::Str(at.repo.clone()))
            .set("agent", Value::Str(agent.to_string()))
            .set("commit", opt_str(&commit))
            .set("tree", Value::Str(tree.clone()))
            .set("runner", Value::Str(piece.runner.clone()))
            .set("command", Value::Str(command.clone()))
            .set("tests", Value::Int(tests.len() as i64))
            .set("passed", counts.map_or(Value::Null, |c| Value::Int(c.passed)))
            .set("failed", counts.map_or(Value::Null, |c| Value::Int(c.failed)))
            .set("skipped", counts.map_or(Value::Null, |c| Value::Int(c.skipped)))
            .set("complete", Value::Bool(complete))
            .set("exit_code", Value::Int(piece.exit_code))
            .set("ms", opt_int(run_ms))
            .set("wait_ms", opt_int(piece_wait))
            .set("source", Value::Str(piece.source.into()))
            .set("use_id", opt_str(&call.use_id))
            .set("ts", ts(at_ms))
            .set("seq", opt_int(seq));
        let _ = sup.db().query("DELETE FROM test_runs WHERE run = :run", &Params::new().set("run", Value::Str(run.clone())));
        // A test without its own time gets the run's when it is the run's only
        // test (marked so: `own_ms` false).
        let alone = tests.len() == 1;
        for (i, t) in tests.iter().enumerate() {
            let own = t.ms.is_some();
            let p = Params::new()
                .set("id", Value::Str(format!("{run}:{i:05}")))
                .set("run", Value::Str(run.clone()))
                .set("repo", Value::Str(at.repo.clone()))
                .set("agent", Value::Str(agent.to_string()))
                .set("commit", opt_str(&commit))
                .set("tree", Value::Str(tree.clone()))
                .set("runner", Value::Str(piece.runner.clone()))
                .set("command", Value::Str(command.clone()))
                .set("test", Value::Str(t.name.clone()))
                .set("outcome", Value::Str(t.outcome.to_string()))
                .set("passed", Value::Bool(t.outcome == "passed"))
                .set("ms", opt_int(t.ms.or(if alone || piece.total_ms.is_some() { run_ms } else { None })))
                .set("own_ms", Value::Bool(own))
                .set("run_ms", opt_int(run_ms))
                .set("exit_code", Value::Int(piece.exit_code))
                .set("error", opt_str(&t.error))
                .set("use_id", opt_str(&call.use_id))
                .set("ts", ts(at_ms))
                .set("seq", opt_int(seq))
                .set("wait_ms", opt_int(piece_wait));
            let _ = sup.db().query(
                "INSERT OR REPLACE INTO test_runs (id, run, repo, agent, \"commit\", tree, runner, command, test, outcome, passed, ms, own_ms, run_ms, exit_code, error, use_id, ts, seq, wait_ms) VALUES (:id, :run, :repo, :agent, :commit, :tree, :runner, :command, :test, :outcome, :passed, :ms, :own_ms, :run_ms, :exit_code, :error, :use_id, :ts, :seq, :wait_ms)",
                &p,
            );
        }
        // The summary goes in last: a reader that sees the command sees its
        // tests too (they were written first).
        let _ = sup.db().query(
            "INSERT OR REPLACE INTO test_summaries (id, repo, agent, \"commit\", tree, runner, command, tests, passed, failed, skipped, complete, exit_code, ms, wait_ms, source, use_id, ts, seq) VALUES (:id, :repo, :agent, :commit, :tree, :runner, :command, :tests, :passed, :failed, :skipped, :complete, :exit_code, :ms, :wait_ms, :source, :use_id, :ts, :seq)",
            &sp,
        );
    }
}

/// At a turn's end: its Bash test calls that fired no `PostToolUse` (they
/// failed, and Claude Code fires none then) are read from the transcript.
fn recover_turn(sup: &Supervisor, agent: &str, transcript: &str) {
    let evs = recent_events(sup, agent, 600);
    let mut turn = Vec::new();
    for e in &evs {
        if e.2 == "UserPromptSubmit" {
            break;
        }
        turn.push(e);
    }
    let done: HashSet<&str> = turn.iter().filter(|e| e.2.starts_with("PostToolUse")).filter_map(|e| e.3["tool_use_id"].as_str()).collect();
    let open: Vec<(String, String)> = turn
        .iter()
        .filter(|e| e.2 == "PreToolUse" && e.3["tool_name"] == "Bash" && is_test_command(e.3["tool_input"]["command"].as_str().unwrap_or("")))
        .filter_map(|e| {
            let u = e.3["tool_use_id"].as_str()?;
            (!done.contains(u)).then(|| (u.to_string(), e.3["tool_input"]["command"].as_str().unwrap_or("").to_string()))
        })
        .collect();
    if open.is_empty() || transcript.is_empty() {
        return;
    }
    let results = tool_results(Path::new(transcript), &open.iter().map(|o| o.0.as_str()).collect::<Vec<_>>());
    for (use_id, command) in open {
        let Some((text, is_error, at)) = results.get(&use_id) else { continue };
        let exists = sup.db().query("SELECT id FROM test_summaries WHERE use_id = :u LIMIT 1", &Params::new().set("u", Value::Str(use_id.clone()))).map(|r| !r.is_empty()).unwrap_or(false);
        if exists {
            continue;
        }
        let exit_code = if *is_error { exit_code_in(text) } else { 0 };
        record(sup, agent, Call { command, output: text.clone(), exit_code, use_id: Some(use_id), end_ms: at.unwrap_or_else(now_ms) });
    }
}

/// The `tool_result`s of some tool calls in a transcript's tail: their
/// text, whether they are errors, and when (ms).
fn tool_results(path: &Path, ids: &[&str]) -> HashMap<String, (String, bool, Option<i64>)> {
    let mut out = HashMap::new();
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return out;
    }
    let Ok(mut f) = std::fs::File::open(path) else { return out };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > TRANSCRIPT_TAIL && f.seek(SeekFrom::Start(len - TRANSCRIPT_TAIL)).is_err() {
        return out;
    }
    let mut buf = Vec::new();
    if f.take(TRANSCRIPT_TAIL).read_to_end(&mut buf).is_err() {
        return out;
    }
    for line in String::from_utf8_lossy(&buf).lines() {
        if !line.contains("tool_result") || !ids.iter().any(|id| line.contains(id)) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Json>(line) else { continue };
        let at = v["timestamp"].as_str().and_then(chloedb::timestamp::parse).map(|t| t / 1000);
        for c in v["message"]["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "tool_result") {
            if let Some(id) = c["tool_use_id"].as_str().filter(|id| ids.contains(id)) {
                out.insert(id.to_string(), (text_of(&c["content"]), c["is_error"] == true, at));
            }
        }
    }
    out
}

// ------------------------------------------------------------------ stats

/// A row of `test_runs` as the stats read it.
#[derive(Clone, Debug)]
struct Row {
    test: String,
    run: String,
    agent: String,
    commit: Option<String>,
    tree: String,
    outcome: String,
    ms: Option<i64>,
    own_ms: bool,
    ts: i64,
    runner: String,
    command: String,
    exit_code: Option<i64>,
    seq: Option<i64>,
    error: Option<String>,
    use_id: Option<String>,
    wait_ms: Option<i64>,
}

const ROW_COLS: &str = "test, run, agent, \"commit\", tree, outcome, ms, own_ms, ts, runner, command, exit_code, seq, error, use_id, wait_ms";

fn row_of(r: &[Value]) -> Row {
    Row {
        test: get_str(&r[0]).unwrap_or_default(),
        run: get_str(&r[1]).unwrap_or_default(),
        agent: get_str(&r[2]).unwrap_or_default(),
        commit: get_str(&r[3]),
        tree: get_str(&r[4]).unwrap_or_default(),
        outcome: get_str(&r[5]).unwrap_or_default(),
        ms: get_int(&r[6]),
        own_ms: get_int(&r[7]).unwrap_or(0) != 0,
        ts: get_ms(&r[8]).unwrap_or(0),
        runner: get_str(&r[9]).unwrap_or_default(),
        command: get_str(&r[10]).unwrap_or_default(),
        exit_code: get_int(&r[11]),
        seq: get_int(&r[12]),
        error: get_str(&r[13]),
        use_id: get_str(&r[14]),
        wait_ms: get_int(&r[15]),
    }
}

/// Rows of `test_runs`, oldest first. Rows an earlier build made of a
/// whole command (runner `other`) are not tests and are left out.
fn rows_where(sup: &Supervisor, cond: &str, p: &Params, limit: usize) -> Vec<Row> {
    sup.db().query(&format!("SELECT {ROW_COLS} FROM test_runs WHERE {cond} AND runner != 'other' ORDER BY ts ASC LIMIT {limit}"), p).unwrap_or_default().iter().map(|r| row_of(r)).collect()
}

/// A row of `test_summaries`: one test command.
#[derive(Clone, Debug)]
struct Sum {
    run: String,
    agent: String,
    commit: Option<String>,
    runner: String,
    command: String,
    tests: i64,
    counts: Option<Counts>,
    complete: bool,
    exit_code: i64,
    ms: Option<i64>,
    wait_ms: Option<i64>,
    source: String,
    use_id: Option<String>,
    ts: i64,
    seq: Option<i64>,
}

const SUM_COLS: &str = "id, agent, \"commit\", runner, command, tests, passed, failed, skipped, complete, exit_code, ms, wait_ms, source, use_id, ts, seq";

fn sums_where(sup: &Supervisor, cond: &str, p: &Params, limit: usize) -> Vec<Sum> {
    let rows = sup.db().query(&format!("SELECT {SUM_COLS} FROM test_summaries WHERE {cond} ORDER BY ts ASC LIMIT {limit}"), p).unwrap_or_default();
    rows.iter()
        .map(|r| Sum {
            run: get_str(&r[0]).unwrap_or_default(),
            agent: get_str(&r[1]).unwrap_or_default(),
            commit: get_str(&r[2]),
            runner: get_str(&r[3]).unwrap_or_default(),
            command: get_str(&r[4]).unwrap_or_default(),
            tests: get_int(&r[5]).unwrap_or(0),
            counts: get_int(&r[6]).map(|passed| Counts { passed, failed: get_int(&r[7]).unwrap_or(0), skipped: get_int(&r[8]).unwrap_or(0) }),
            complete: get_int(&r[9]).unwrap_or(0) != 0,
            exit_code: get_int(&r[10]).unwrap_or(0),
            ms: get_int(&r[11]),
            wait_ms: get_int(&r[12]),
            source: get_str(&r[13]).unwrap_or_default(),
            use_id: get_str(&r[14]),
            ts: get_ms(&r[15]).unwrap_or(0),
            seq: get_int(&r[16]),
        })
        .collect()
}

/// A run Workbench made itself ("Run it N times"), not an agent.
fn by_workbench(use_id: &Option<String>) -> bool {
    use_id.as_deref().is_some_and(|u| u.starts_with(RERUN_PREFIX))
}

/// A test's verdicts over its runs (oldest first).
#[derive(Debug, Default, Clone)]
pub struct Verdict {
    pub flaky: bool,
    pub flaky_why: Option<String>,
    pub newly_failing: bool,
    /// Its failures an agent fixed in its session: a failure, then a pass
    /// by the same agent after its code changed. Not counted against the
    /// pass rate (unless the test is flaky).
    pub fixed: usize,
    /// "On <commit>: N runs, 0 failures": its runs on the code of its last
    /// normal run, when none failed, against the code it last failed on.
    pub stable: Option<Json>,
    /// Its runs per code (commit, changes, environment), newest first.
    pub codes: Vec<Json>,
}

fn short(sha: &Option<String>) -> String {
    sha.as_deref().map(|s| s.chars().take(8).collect()).unwrap_or_else(|| "no commit".into())
}

/// The failures of `decided` (indices) an agent fixed in its session.
fn fixed_failures(decided: &[&&Row]) -> HashSet<usize> {
    let mut out = HashSet::new();
    for (i, r) in decided.iter().enumerate().filter(|(_, r)| r.outcome == "failed") {
        let later_pass = decided[i + 1..].iter().any(|p| p.outcome == "passed" && p.agent == r.agent && (p.commit != r.commit || p.tree != r.tree));
        if later_pass {
            out.insert(i);
        }
    }
    out
}

/// The failures of a test's runs that an agent fixed in its session, by
/// run id: what the pass rate leaves out, and Ship's "fixed in this
/// session" (empty when the test is flaky: then its failures count).
pub fn fixed_in_session(sup: &Supervisor, repo: &str, test: &str) -> Vec<String> {
    let p = Params::new().set("repo", Value::Str(repo.to_string())).set("test", Value::Str(test.to_string()));
    let rows = rows_where(sup, "repo = :repo AND test = :test", &p, 100_000);
    let refs: Vec<&Row> = rows.iter().collect();
    if verdict(&refs).flaky {
        return Vec::new();
    }
    let decided: Vec<&&Row> = refs.iter().filter(|r| r.outcome == "passed" || r.outcome == "failed").collect();
    let fixed = fixed_failures(&decided);
    decided.iter().enumerate().filter(|(i, _)| fixed.contains(i)).map(|(_, r)| r.run.clone()).collect()
}

/// "The same code" for a test's runs: its commit, its uncommitted changes
/// and the environment its command set for the runner ([`variant`]).
type CodeKey = (String, String, String);

fn code_key(r: &Row, variants: &mut HashMap<String, String>) -> CodeKey {
    let v = variants.entry(r.command.clone()).or_insert_with(|| variant(&r.command)).clone();
    (r.commit.clone().unwrap_or_default(), r.tree.clone(), v)
}

/// A test's runs on each code, newest first: its commit, uncommitted
/// changes, environment, runs, passes and failures, and who ran it.
fn codes_json(decided: &[&&Row]) -> Vec<Json> {
    let mut variants = HashMap::new();
    let mut by: Vec<(CodeKey, usize, usize, i64, i64, Vec<String>, usize)> = Vec::new();
    for r in decided {
        let k = code_key(r, &mut variants);
        let i = match by.iter().position(|b| b.0 == k) {
            Some(i) => i,
            None => {
                by.push((k, 0, 0, r.ts, r.ts, Vec::new(), 0));
                by.len() - 1
            }
        };
        let e = &mut by[i];
        if r.outcome == "passed" {
            e.1 += 1;
        } else {
            e.2 += 1;
        }
        e.4 = e.4.max(r.ts);
        if !r.agent.is_empty() && !e.5.contains(&r.agent) {
            e.5.push(r.agent.clone());
        }
        e.6 += by_workbench(&r.use_id) as usize;
    }
    by.sort_by(|a, b| b.4.cmp(&a.4));
    by.into_iter()
        .map(|((commit, tree, env), p, f, first, last, agents, wb)| {
            json!({ "commit": Some(commit).filter(|c| !c.is_empty()), "tree": tree, "env": Some(env).filter(|e| !e.is_empty()), "runs": p + f, "passed": p, "failed": f, "first_ms": first, "last_ms": last, "agents": agents, "by_workbench": wb })
        })
        .collect()
}

/// How many passes in a row, on new code, tell a fix from luck when the
/// old code failed `failed` of `runs`: a test still failing that often
/// passes n times in a row less than 5% of the time.
fn passes_needed(failed: usize, runs: usize) -> usize {
    if failed == 0 || runs == 0 {
        return 0;
    }
    let f = failed as f64 / runs as f64;
    if f >= 1.0 {
        return 1;
    }
    ((0.05f64).ln() / (1.0 - f).ln()).ceil().max(1.0) as usize
}

/// With n passes and no failure, the failure rate it is below with 95%
/// confidence (1 − 0.05^(1/n); about 3/n).
fn upper_95(n: usize) -> Option<f64> {
    (n > 0).then(|| 1.0 - (0.05f64).powf(1.0 / n as f64))
}

/// Flaky: passed and failed on the same code (commit, uncommitted changes
/// and the environment its command set). A failure, then a pass after
/// the code changed, is a fix, not a flake. Newly failing: its last run
/// failed after a pass. Stable: the runs on the code of its last normal
/// run (no environment of its own), when none of them failed; never the
/// runs of other code.
fn verdict(runs: &[&Row]) -> Verdict {
    let mut v = Verdict::default();
    let decided: Vec<&&Row> = runs.iter().filter(|r| r.outcome == "passed" || r.outcome == "failed").collect();
    let mut variants = HashMap::new();
    let keys: Vec<CodeKey> = decided.iter().map(|r| code_key(r, &mut variants)).collect();
    // Per code: passes, failures and its last run's index.
    let mut by_code: HashMap<&CodeKey, (usize, usize, usize)> = HashMap::new();
    for (i, (r, k)) in decided.iter().zip(&keys).enumerate() {
        let e = by_code.entry(k).or_default();
        if r.outcome == "passed" {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
        e.2 = i;
    }
    let fixed = fixed_failures(&decided);
    // The flaky code seen last.
    if let Some(((commit, _, env), (p, f, _))) = by_code.iter().filter(|(_, (p, f, _))| *p > 0 && *f > 0).max_by_key(|(_, e)| e.2) {
        v.flaky = true;
        let env = if env.is_empty() { String::new() } else { format!(" (with {env})") };
        v.flaky_why = Some(format!("passed and failed on {} with the same code{env}: {f} of {} runs failed", short(&Some(commit.clone()).filter(|c| !c.is_empty())), p + f));
    }
    if !v.flaky {
        v.fixed = fixed.len();
    }
    // Newly failing: by its normal runs (an environment of its own aside).
    let normal: Vec<&&Row> = decided.iter().zip(&keys).filter(|(_, k)| k.2.is_empty()).map(|(r, _)| *r).collect();
    let basis = if normal.is_empty() { &decided } else { &normal };
    if let Some(last) = basis.last() {
        v.newly_failing = last.outcome == "failed" && basis.iter().any(|r| r.outcome == "passed");
    }
    // Stable: the code of its last normal run, when it never failed there.
    let here = keys.iter().rposition(|k| k.2.is_empty()).or_else(|| keys.len().checked_sub(1));
    if let Some(h) = here {
        let key = &keys[h];
        let (p, f, _) = by_code[key];
        if f == 0 && p > 0 {
            let on: Vec<&&&Row> = decided.iter().zip(&keys).filter(|(_, k)| *k == key).map(|(r, _)| r).collect();
            // The code it last failed on (normal runs), for comparison.
            let before = decided.iter().zip(&keys).rev().find(|(r, k)| r.outcome == "failed" && *k != key && k.2.is_empty()).map(|(_, k)| k);
            let (bp, bf) = before.map_or((0, 0), |k| (by_code[k].0, by_code[k].1));
            let needed = passes_needed(bf, bp + bf);
            v.stable = Some(json!({
                "since_commit": Some(key.0.clone()).filter(|c| !c.is_empty()), "since_ms": on.first().map(|r| r.ts), "since_tree": key.1, "env": Some(key.2.clone()).filter(|e| !e.is_empty()),
                "runs": p, "failures": 0, "after_failure": before.is_some(),
                "before_commit": before.map(|k| k.0.clone()).filter(|c| !c.is_empty()), "before_tree": before.map(|k| k.1.clone()),
                "failed_before": bf, "runs_before": bp + bf,
                "needed": (needed > 0).then_some(needed), "enough": (needed > 0).then_some(p >= needed),
                "upper_95": upper_95(p).map(|u| (u * 1000.0).round() / 1000.0),
                "by_workbench": on.iter().filter(|r| by_workbench(&r.use_id)).count(),
            }));
        }
    }
    v.codes = codes_json(&decided);
    v
}

/// The nearest-rank percentile of sorted values.
fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

fn is_slow(p50: Option<i64>, p99: Option<i64>) -> bool {
    match (p50, p99) {
        (_, Some(p99)) if p99 >= 5000 => true,
        (Some(p50), Some(p99)) => p99 >= 1000 && p99 >= 3 * p50.max(1),
        _ => false,
    }
}

/// p50 and p99 per test with ChloeDB's `approx_percentile`; `None` when
/// the statement does not run (then they are computed here).
fn sql_percentiles(sup: &Supervisor, repo: &str, since: i64) -> Option<HashMap<String, (Option<i64>, Option<i64>)>> {
    let p = Params::new().set("repo", Value::Str(repo.to_string())).set("since", ts(since));
    let rows = sup
        .db()
        .query("SELECT test, approx_percentile(ms, 0.5) AS p50, approx_percentile(ms, 0.99) AS p99 FROM test_runs WHERE repo = :repo AND ts >= :since AND ms IS NOT NULL AND runner != 'other' GROUP BY test LIMIT 100000", &p)
        .ok()?;
    let f = |v: &Value| match v {
        Value::Float(x) => Some(x.round() as i64),
        other => get_int(other),
    };
    Some(rows.iter().filter_map(|r| Some((get_str(&r[0])?, (f(&r[1]), f(&r[2]))))).collect())
}

/// One test's numbers and verdicts, as `/api/tests` answers them. The
/// pass rate leaves out the failures an agent fixed in its session; slow
/// is only said of a test's own time (not its command's, nor a wait).
fn test_json(test: &str, runs: &[&Row], pct: Option<(Option<i64>, Option<i64>)>) -> (Json, Verdict) {
    let passed = runs.iter().filter(|r| r.outcome == "passed").count();
    let failed = runs.iter().filter(|r| r.outcome == "failed").count();
    let (p50, p99) = pct.unwrap_or_else(|| {
        let mut ms: Vec<i64> = runs.iter().filter_map(|r| r.ms).collect();
        ms.sort_unstable();
        (percentile(&ms, 0.5), percentile(&ms, 0.99))
    });
    let v = verdict(runs);
    let counted = (passed + failed).saturating_sub(v.fixed);
    let last = runs.last().copied();
    let mut agents: Vec<&str> = Vec::new();
    for r in runs {
        if !agents.contains(&r.agent.as_str()) {
            agents.push(&r.agent);
        }
    }
    let own = runs.iter().any(|r| r.own_ms);
    let history: Vec<Json> = runs[runs.len().saturating_sub(20)..].iter().map(|r| json!({ "outcome": r.outcome, "ts_ms": r.ts, "agent": r.agent, "commit": r.commit, "ms": r.ms, "by_workbench": by_workbench(&r.use_id) })).collect();
    let j = json!({
        "test": test, "runner": last.map(|r| r.runner.clone()), "runs": runs.len(), "passed": passed, "failed": failed,
        "fixed": v.fixed, "pass_rate": if counted > 0 { passed as f64 / counted as f64 } else { 1.0 }, "p50_ms": p50, "p99_ms": p99, "own_ms": own,
        "flaky": v.flaky, "flaky_why": v.flaky_why, "slow": own && is_slow(p50, p99), "newly_failing": v.newly_failing, "stable": v.stable,
        "last": last.map(|r| json!({ "outcome": r.outcome, "ts_ms": r.ts, "ts": iso(r.ts), "agent": r.agent, "commit": r.commit, "command": r.command })),
        "agents": agents, "history": history,
    });
    (j, v)
}

/// Test commands, newest first: each one's counts (from its summary row),
/// the tests that failed in it, and how long it waited for a permission.
fn commands_of(sums: &[Sum], rows: &[Row], newly: &HashSet<String>, flaky: &HashSet<String>) -> Vec<Json> {
    let mut by_run: HashMap<&str, Vec<&Row>> = HashMap::new();
    for r in rows {
        by_run.entry(r.run.as_str()).or_default().push(r);
    }
    let mut out: Vec<(i64, Json)> = sums
        .iter()
        .map(|s| {
            let rs = by_run.get(s.run.as_str()).cloned().unwrap_or_default();
            let failed_tests: Vec<&str> = rs.iter().filter(|r| r.outcome == "failed").map(|r| r.test.as_str()).collect();
            let c = s.counts;
            (
                s.ts,
                json!({
                    "run": s.run, "agent": s.agent, "command": s.command, "runner": s.runner, "ts_ms": s.ts, "ts": iso(s.ts),
                    "exit_code": s.exit_code, "tests": c.map_or(s.tests, |c| c.total().max(s.tests)), "named": s.tests,
                    "passed": c.map(|c| c.passed), "failed": c.map(|c| c.failed), "ignored": c.map(|c| c.skipped),
                    "counted": c.is_some(), "complete": s.complete, "source": s.source,
                    "ms": s.ms, "wait_ms": s.wait_ms, "commit": s.commit,
                    "seq": s.seq, "seqs": s.seq.into_iter().collect::<Vec<_>>(), "failed_tests": failed_tests, "running": false,
                    "by_workbench": by_workbench(&s.use_id),
                    "newly_failing": failed_tests.iter().filter(|t| newly.contains(**t)).collect::<Vec<_>>(),
                    "flaky_tests": rs.iter().map(|r| r.test.as_str()).filter(|t| flaky.contains(*t)).collect::<Vec<_>>(),
                }),
            )
        })
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, j)| j).collect()
}

/// Tests no longer run: a later complete run of the same command (every
/// test it counted is named) did not have them (deleted or renamed).
fn gone_tests(sums: &[Sum], rows: &[Row], last_of: &HashMap<&str, &Row>) -> HashSet<String> {
    let mut names: HashMap<&str, HashSet<&str>> = HashMap::new();
    for r in rows {
        names.entry(r.run.as_str()).or_default().insert(r.test.as_str());
    }
    let mut out = HashSet::new();
    for (test, last) in last_of {
        let gone = sums.iter().any(|s| s.complete && s.command == last.command && s.ts > last.ts && !names.get(s.run.as_str()).is_some_and(|n| n.contains(test)));
        if gone {
            out.insert(test.to_string());
        }
    }
    out
}

/// `GET /api/tests?repo=&days=7`: the radar of a repository.
pub fn radar(sup: &Supervisor, repo: Option<&str>, days: i64) -> Result<Json, ApiError> {
    let repo = repo_of(sup, repo)?;
    let days = days.clamp(1, 90);
    let now = now_ms();
    let since = now - days * 86_400_000;
    let p = Params::new().set("repo", Value::Str(repo.clone())).set("since", ts(since));
    let rows = rows_where(sup, "repo = :repo AND ts >= :since", &p, 200_000);
    let sums = sums_where(sup, "repo = :repo AND ts >= :since", &p, 50_000);
    let pcts = sql_percentiles(sup, &repo, since);
    let mut by_test: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for r in &rows {
        by_test.entry(r.test.as_str()).or_default().push(r);
    }
    let last_of: HashMap<&str, &Row> = by_test.iter().filter_map(|(t, rs)| Some((*t, *rs.last()?))).collect();
    let gone = gone_tests(&sums, &rows, &last_of);
    let mut tests = Vec::new();
    let (mut newly, mut flaky) = (HashSet::new(), HashSet::new());
    let mut fixed = 0;
    for (test, runs) in &by_test {
        let (mut j, v) = test_json(test, runs, pcts.as_ref().and_then(|m| m.get(*test).copied()));
        j["gone"] = json!(gone.contains(*test));
        if v.newly_failing && !gone.contains(*test) {
            newly.insert(test.to_string());
        } else if v.newly_failing {
            j["newly_failing"] = json!(false);
        }
        if v.flaky {
            flaky.insert(test.to_string());
        }
        fixed += v.fixed;
        tests.push(j);
    }
    let commands = commands_of(&sums, &rows, &newly, &flaky);
    let count = |k: &str| tests.iter().filter(|t| t[k] == true).count();
    let passed = rows.iter().filter(|r| r.outcome == "passed").count();
    let failed = rows.iter().filter(|r| r.outcome == "failed").count();
    let counted = (passed + failed).saturating_sub(fixed);
    // Commands that named no test: only their counts (or exit codes).
    let quiet: Vec<&Sum> = sums.iter().filter(|s| s.tests == 0).collect();
    let quiet_counts = quiet.iter().filter_map(|s| s.counts).fold(Counts::default(), |mut a, c| {
        a.add(c);
        a
    });
    let mut agent_ids: Vec<&str> = rows.iter().map(|r| r.agent.as_str()).chain(sums.iter().map(|s| s.agent.as_str())).collect();
    agent_ids.sort_unstable();
    agent_ids.dedup();
    let agents: Vec<Json> = agent_ids.iter().filter_map(|id| sup.agent_json_of(id)).collect();
    let mut missing = Vec::new();
    if pcts.is_none() && !rows.is_empty() {
        missing.push("approx_percentile did not run here, so p50 and p99 were computed from the rows".to_string());
    }
    Ok(json!({
        "repo": repo,
        "window": { "from_ms": since, "to_ms": now, "days": days },
        "totals": { "tests": tests.len(), "runs": rows.len(), "commands": sums.len(), "passed": passed, "failed": failed, "fixed": fixed,
                    "pass_rate": if counted > 0 { passed as f64 / counted as f64 } else { 1.0 },
                    "flaky": count("flaky"), "slow": count("slow"), "newly_failing": newly.len(), "gone": gone.len(),
                    "quiet": { "commands": quiet.len(), "counted": quiet.iter().filter(|s| s.counts.is_some()).count(), "passed": quiet_counts.passed, "failed": quiet_counts.failed, "skipped": quiet_counts.skipped },
                    "waited_ms": sums.iter().filter_map(|s| s.wait_ms).sum::<i64>() },
        "tests": tests,
        "commands": commands.into_iter().take(50).collect::<Vec<_>>(),
        "agents": agents,
        "percentiles": if pcts.is_some() { "approx_percentile" } else { "rust" },
        "missing": missing,
    }))
}

/// `GET /api/tests/history?repo=&test=`: a test's runs, newest first.
pub fn history(sup: &Supervisor, repo: Option<&str>, test: &str) -> Result<Json, ApiError> {
    let repo = repo_of(sup, repo)?;
    let p = Params::new().set("repo", Value::Str(repo.clone())).set("test", Value::Str(test.to_string()));
    let rows = rows_where(sup, "repo = :repo AND test = :test", &p, 100_000);
    if rows.is_empty() {
        return Err(ApiError::not_found(format!("no runs of {test} in {repo}")));
    }
    let refs: Vec<&Row> = rows.iter().collect();
    let v = verdict(&refs);
    let fixed: HashSet<String> = fixed_in_session(sup, &repo, test).into_iter().collect();
    let mut agent_ids: Vec<&str> = rows.iter().map(|r| r.agent.as_str()).collect();
    agent_ids.sort_unstable();
    agent_ids.dedup();
    let runs: Vec<Json> = rows
        .iter()
        .rev()
        .take(500)
        .map(|r| json!({ "run": r.run, "agent": r.agent, "outcome": r.outcome, "ms": r.ms, "own_ms": r.own_ms, "wait_ms": r.wait_ms, "ts_ms": r.ts, "ts": iso(r.ts), "commit": r.commit, "tree": r.tree, "command": r.command, "exit_code": r.exit_code, "seq": r.seq, "error": r.error, "fixed": fixed.contains(r.run.as_str()), "by_workbench": by_workbench(&r.use_id) }))
        .collect();
    let last = rows.last();
    // The checkout Run it N times runs in by default: the one of the
    // newest run whose checkout is still there.
    let checkouts = checkouts_of(sup, &repo);
    let default_checkout = rows.iter().rev().map(|r| if r.agent.is_empty() { MAIN_CHECKOUT.to_string() } else { r.agent.clone() }).find(|c| checkouts.iter().any(|x| x["id"] == json!(c))).unwrap_or_else(|| MAIN_CHECKOUT.into());
    Ok(json!({
        "test": test, "repo": repo, "runner": last.map(|r| r.runner.clone()), "runs": runs,
        "flaky": v.flaky, "flaky_why": v.flaky_why, "newly_failing": v.newly_failing, "fixed": v.fixed, "stable": v.stable, "codes": v.codes,
        "rerun": rerun_of(&rows, test).map(|c| shell_join(&c)),
        "checkouts": checkouts, "checkout": default_checkout,
        "agents": agent_ids.iter().filter_map(|id| sup.agent_json_of(id)).collect::<Vec<_>>(),
    }))
}

/// The words that run one test again, from the newest run of it whose
/// command Workbench can aim at that test.
fn rerun_of(rows: &[Row], test: &str) -> Option<Vec<String>> {
    rows.iter().rev().find_map(|r| rerun_command(&r.runner, &r.command, test))
}

/// Words as a shell line: quoted where a shell would split or expand them.
pub fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|w| if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c)) { w.clone() } else { format!("'{}'", w.replace('\'', "'\\''")) })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The id of the project's own checkout among the places a test runs.
const MAIN_CHECKOUT: &str = "main";

/// Where Run it N times can run: the project's own checkout and each
/// agent's worktree that is still there, with its branch and commit.
fn checkouts_of(sup: &Supervisor, repo: &str) -> Vec<Json> {
    let mut out = Vec::new();
    let main = Path::new(repo);
    if main.is_dir() {
        let (commit, tree) = code_of(main);
        out.push(json!({ "id": MAIN_CHECKOUT, "label": "the project's checkout", "dir": repo, "branch": crate::git::current_branch(main), "commit": commit, "dirty": tree != "clean", "tree": tree }));
    }
    let mut seen = HashSet::new();
    let mut agents = sup.agent_rows();
    agents.sort_by_key(|a| std::cmp::Reverse(a.created));
    for a in agents.into_iter().filter(|a| a.repo == repo && a.cwd != repo) {
        if !seen.insert(a.cwd.clone()) || !Path::new(&a.cwd).is_dir() {
            continue;
        }
        let (commit, tree) = code_of(Path::new(&a.cwd));
        out.push(json!({ "id": a.id, "label": format!("{}'s worktree", a.name), "dir": a.cwd, "branch": a.branch, "commit": commit, "dirty": tree != "clean", "tree": tree }));
        if out.len() >= 12 {
            break;
        }
    }
    out
}

/// `GET /api/tests/runs?agent=`: an agent's test commands, newest first,
/// with what failed, what is flaky and what newly fails (Story's chip).
pub fn agent_runs(sup: &Supervisor, agent: &str) -> Result<Json, ApiError> {
    let a = sup.agent_row(agent).ok_or_else(|| ApiError::not_found(format!("no agent {agent}")))?;
    let p = Params::new().set("agent", Value::Str(agent.to_string()));
    let rows = rows_where(sup, "agent = :agent", &p, 100_000);
    let sums = sums_where(sup, "agent = :agent", &p, 10_000);
    // Verdicts over the repository's last week, for the tests this agent ran.
    let names: HashSet<&str> = rows.iter().map(|r| r.test.as_str()).collect();
    let since = now_ms() - 7 * 86_400_000;
    let rp = Params::new().set("repo", Value::Str(a.repo.clone())).set("since", ts(since));
    let all = rows_where(sup, "repo = :repo AND ts >= :since", &rp, 200_000);
    let mut by_test: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for r in all.iter().filter(|r| names.contains(r.test.as_str())) {
        by_test.entry(r.test.as_str()).or_default().push(r);
    }
    let (mut newly, mut flaky) = (HashSet::new(), HashSet::new());
    for (t, runs) in by_test {
        let v = verdict(&runs);
        if v.newly_failing {
            newly.insert(t.to_string());
        }
        if v.flaky {
            flaky.insert(t.to_string());
        }
    }
    Ok(json!({ "runs": commands_of(&sums, &rows, &newly, &flaky) }))
}

/// Tests that failed in a window, for an incident's board: per test of
/// the agent (or every agent), its last 20 runs.
pub fn board_tests(sup: &Supervisor, repo: &str, tests: &[String]) -> Vec<Json> {
    let mut out = Vec::new();
    for t in tests.iter().take(6) {
        let p = Params::new().set("repo", Value::Str(repo.to_string())).set("test", Value::Str(t.clone()));
        let rows = rows_where(sup, "repo = :repo AND test = :test", &p, 10_000);
        if rows.is_empty() {
            continue;
        }
        let refs: Vec<&Row> = rows.iter().collect();
        let (mut j, _) = test_json(t, &refs, None);
        j["error"] = json!(rows.iter().rev().find_map(|r| r.error.clone()));
        out.push(j);
    }
    out
}

/// Where a test passes and where it fails now, checkout by checkout: the
/// runs on the code each checkout holds (its commit and uncommitted
/// changes; runs with an environment of their own aside), and a sentence:
/// "Fixed on wb/flake-hunt (9f098a11), not merged: 30 runs, no failure;
/// main (033fb7ff) still fails: 2 of 27 runs failed." `fixed_by`: the
/// agents whose worktree holds code it only passed on while the project's
/// checkout fails. `None` when the test has no runs.
pub fn where_now(sup: &Supervisor, repo: &str, test: &str) -> Option<Json> {
    // Asked on each list of alerts: its git calls at most every 15 s.
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<(String, String), (std::time::Instant, Option<Json>)>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (repo.to_string(), test.to_string());
    if let Some((at, v)) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if at.elapsed() < std::time::Duration::from_secs(15) {
            return v.clone();
        }
    }
    let v = where_now_uncached(sup, repo, test);
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    if c.len() > 500 {
        c.clear();
    }
    c.insert(key, (std::time::Instant::now(), v.clone()));
    v
}

fn where_now_uncached(sup: &Supervisor, repo: &str, test: &str) -> Option<Json> {
    let p = Params::new().set("repo", Value::Str(repo.to_string())).set("test", Value::Str(test.to_string()));
    let rows = rows_where(sup, "repo = :repo AND test = :test", &p, 100_000);
    if rows.is_empty() {
        return None;
    }
    let mut by_code: HashMap<(String, String), (usize, usize)> = HashMap::new();
    let mut variants: HashMap<String, String> = HashMap::new();
    for r in rows.iter().filter(|r| r.outcome == "passed" || r.outcome == "failed") {
        if !code_key(r, &mut variants).2.is_empty() {
            continue;
        }
        let e = by_code.entry((r.commit.clone().unwrap_or_default(), r.tree.clone())).or_default();
        if r.outcome == "passed" {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }
    struct Here {
        id: String,
        label: String,
        branch: Option<String>,
        commit: Option<String>,
        tree: String,
        runs: (usize, usize),
    }
    let mut places: Vec<Here> = Vec::new();
    for c in checkouts_of(sup, repo) {
        let commit = c["commit"].as_str().map(str::to_string);
        let tree = c["tree"].as_str().unwrap_or("clean").to_string();
        let runs = by_code.get(&(commit.clone().unwrap_or_default(), tree.clone())).copied().unwrap_or((0, 0));
        places.push(Here { id: c["id"].as_str().unwrap_or("").into(), label: c["label"].as_str().unwrap_or("").into(), branch: c["branch"].as_str().map(str::to_string), commit, tree, runs });
    }
    let main = places.iter().find(|x| x.id == MAIN_CHECKOUT);
    let main_fails = main.is_some_and(|m| m.runs.1 > 0);
    let name = |x: &Here| x.branch.clone().unwrap_or_else(|| x.label.clone());
    // The same code as the project's checkout: not a place of its own.
    let as_main = |x: &Here| main.is_some_and(|m| m.commit == x.commit && m.tree == x.tree);
    let dirty = |x: &Here| if x.tree == "clean" { "" } else { " with uncommitted changes" };
    let mut parts = Vec::new();
    let mut fixed_by = Vec::new();
    let mut fixed_on = Vec::new();
    let mut seen_commits = HashSet::new();
    for x in places.iter().filter(|x| x.id != MAIN_CHECKOUT && x.runs.0 > 0 && x.runs.1 == 0) {
        // The same code as main, or code already said: not a fix of its own.
        if as_main(x) || !seen_commits.insert((x.commit.clone(), x.tree.clone())) {
            continue;
        }
        let merged = match (main.and_then(|m| m.commit.as_deref()), x.commit.as_deref()) {
            (Some(m), Some(c)) => crate::git::is_ancestor(Path::new(repo), c, m),
            _ => false,
        };
        let word = if main_fails { "Fixed on" } else { "Passes on" };
        parts.push(format!("{word} {} ({}){}{}: {} run{}, no failure", name(x), short(&x.commit), dirty(x), if merged { ", merged" } else if main.is_some() { ", not merged" } else { "" }, x.runs.0, if x.runs.0 == 1 { "" } else { "s" }));
        if main_fails {
            fixed_by.push(x.id.clone());
        }
        fixed_on.push(json!({ "checkout": x.id, "branch": x.branch, "commit": x.commit, "runs": x.runs.0, "merged": merged }));
    }
    if let Some(m) = main {
        let (p, f) = m.runs;
        parts.push(match (p, f) {
            (_, f) if f > 0 => format!("{} ({}) still fails: {f} of {} runs failed", name(m), short(&m.commit), p + f),
            (p, 0) if p > 0 => format!("{} ({}) passes: {p} run{}, no failure", name(m), short(&m.commit), if p == 1 { "" } else { "s" }),
            _ => format!("not run on {} ({}) yet", name(m), short(&m.commit)),
        });
    }
    for x in places.iter().filter(|x| x.id != MAIN_CHECKOUT && x.runs.1 > 0 && !as_main(x)).take(3) {
        parts.push(format!("fails on {} ({}){}: {} of {} runs", name(x), short(&x.commit), dirty(x), x.runs.1, x.runs.0 + x.runs.1));
    }
    let mut sentence = parts.join("; ");
    if let Some(c) = sentence.get(..1) {
        sentence = format!("{}{}.", c.to_uppercase(), &sentence[1..]);
    }
    Some(json!({ "test": test, "sentence": sentence, "fixed_by": fixed_by, "fixed_on": fixed_on, "main": main.map(|m| json!({ "branch": m.branch, "commit": m.commit, "passed": m.runs.0, "failed": m.runs.1 })) }))
}

// ------------------------------------------------------- run it N times

/// The `use_id` of the runs Workbench makes itself.
const RERUN_PREFIX: &str = "wb-rerun:";
/// The most runs one "Run it N times" makes, and each one's time limit.
const RERUN_MAX: usize = 50;
const RERUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The words that run one test again, as its runner names it, from the
/// command that last ran it (its wrapper: `python3 -m pytest`, `uv run
/// pytest`, `cargo test -p x`): `None` for a runner Workbench cannot aim
/// at one test.
pub fn rerun_command(runner: &str, command: &str, test: &str) -> Option<Vec<String>> {
    // Each simple command's program and words, without the shell's own
    // words (`do`, `time`), its redirections and the assignments before it.
    let simple: Vec<Vec<String>> = simple_commands(command).into_iter().map(|c| c.words).collect();
    let plain = |w: &str| !w.contains(['$', '`', '(', ')', '<', '>', '*', '{', ';', '|', '&']) && !SHELL_LEAD.contains(&w);
    match runner {
        "pytest" | "junit" => {
            let words = simple.iter().find(|w| pytest_at(w).is_some())?;
            let at = pytest_at(words)?;
            let mut out: Vec<String> = words[..=at].iter().filter(|w| !w.contains('=')).cloned().collect();
            if !out.iter().all(|w| plain(w)) || !test.contains("::") {
                return None;
            }
            out.extend(["-q".to_string(), "-rA".into(), "-p".into(), "no:cacheprovider".into(), test.to_string()]);
            Some(out)
        }
        "cargo" => {
            let words = simple.iter().find(|w| w.len() >= 2 && w[0] == "cargo" && w[1] == "test")?;
            let mut out = vec!["cargo".to_string(), "test".to_string()];
            let mut i = 2;
            while i < words.len() && words[i] != "--" {
                let w = &words[i];
                if matches!(w.as_str(), "-p" | "--package" | "--test" | "--features" | "--manifest-path" | "--target" | "--profile" | "--lib" | "--release" | "--all-features" | "--no-default-features") {
                    out.push(w.clone());
                    if !matches!(w.as_str(), "--lib" | "--release" | "--all-features" | "--no-default-features") {
                        if let Some(v) = words.get(i + 1) {
                            out.push(v.clone());
                            i += 1;
                        }
                    }
                } else if w.starts_with("--package=") || w.starts_with("--features=") || w.starts_with("--profile=") || w.starts_with("--test=") {
                    out.push(w.clone());
                }
                i += 1;
            }
            if !out.iter().all(|w| plain(w)) {
                return None;
            }
            out.extend([test.to_string(), "--".into(), "--exact".into()]);
            Some(out)
        }
        "go" => {
            let (pkg, name) = test.split_once(" › ")?;
            let top = name.split('/').next().unwrap_or(name);
            Some(vec!["go".into(), "test".into(), "-count=1".into(), "-v".into(), "-run".into(), format!("^{top}$"), pkg.to_string()])
        }
        _ => None,
    }
}

/// One "Run it N times" in progress or done, kept while the server runs.
#[derive(Clone, Debug)]
struct Rerun {
    id: String,
    test: String,
    /// The agent whose worktree it runs in; empty for the project's checkout.
    agent: String,
    command: Vec<String>,
    env: Vec<(String, String)>,
    n: usize,
    done: usize,
    passed: usize,
    failed: usize,
    state: &'static str,
    error: Option<String>,
    started_ms: i64,
    commit: Option<String>,
    dirty: bool,
    branch: Option<String>,
    checkout: String,
    checkout_label: String,
}

impl Rerun {
    fn to_json(&self) -> Json {
        let env: Vec<String> = self.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let cmd = shell_join(&self.command);
        json!({ "id": self.id, "test": self.test, "agent": self.agent, "command": if env.is_empty() { cmd } else { format!("{} {cmd}", env.join(" ")) }, "n": self.n, "done": self.done,
                "passed": self.passed, "failed": self.failed, "state": self.state, "error": self.error, "started_ms": self.started_ms, "commit": self.commit,
                "dirty": self.dirty, "branch": self.branch, "checkout": self.checkout, "checkout_label": self.checkout_label })
    }
}

fn reruns() -> &'static std::sync::Mutex<HashMap<String, Rerun>> {
    static R: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Rerun>>> = std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Runs `words` in `dir` (no shell) with `env` and a time limit: its
/// output and exit code. In a process group of its own, so that a test
/// that ends "its" processes (or the time limit) never reaches anything
/// else, and the time limit ends what the test started too.
fn run_once(words: &[String], env: &[(String, String)], dir: &Path) -> Result<(String, i64), String> {
    let mut cmd = Command::new(&words[0]);
    cmd.args(&words[1..])
        .current_dir(dir)
        .env("CI", "1")
        // Workbench's own runs leave the checkout as they found it.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("CHLOEDB_WORKBENCH_TOKEN")
        .env_remove("CHLOEDB_WORKBENCH_AGENT")
        .env_remove("PYTEST_ADDOPTS")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {}: {e}", words[0]))?;
    let (mut out, mut err) = (child.stdout.take(), child.stderr.take());
    let o = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(p) = out.as_mut() {
            let _ = p.take(8 << 20).read_to_end(&mut s);
        }
        s
    });
    let e = std::thread::spawn(move || {
        let mut s = Vec::new();
        if let Some(p) = err.as_mut() {
            let _ = p.take(1 << 20).read_to_end(&mut s);
        }
        s
    });
    let t0 = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(s);
        }
        if t0.elapsed() > RERUN_TIMEOUT {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let text = format!("{}\n{}", String::from_utf8_lossy(&o.join().unwrap_or_default()), String::from_utf8_lossy(&e.join().unwrap_or_default()));
    match status {
        Some(s) => Ok((text, s.code().unwrap_or(1) as i64)),
        None => Err(format!("it ran over {} s and was stopped", RERUN_TIMEOUT.as_secs())),
    }
}

#[derive(serde::Deserialize)]
struct RerunB {
    repo: Option<String>,
    test: String,
    n: Option<usize>,
    /// The agent whose worktree it runs in (the older name of `checkout`).
    agent: Option<String>,
    /// `main` (the project's own checkout) or an agent's id.
    checkout: Option<String>,
    /// The command, as the user edited it: one program and its words,
    /// with `NAME=value`s before it; no shell.
    command: Option<String>,
}

/// A command the user typed for Run it N times: its environment and
/// words. Workbench runs it without a shell, so a line with `;`, `&&`,
/// `|`, a redirection or a substitution is refused, not half run.
fn typed_command(line: &str) -> Result<(Vec<(String, String)>, Vec<String>), String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("the command is empty".into());
    }
    let mut quote = None;
    for c in line.chars() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ';' | '&' | '|' | '<' | '>' | '`' | '\n') => return Err(format!("Workbench runs one command without a shell: `{c}` has no meaning here. Run the test runner itself, like `python3 -m pytest path::test`.")),
            (None, '$') => return Err("Workbench runs one command without a shell: `$` is not expanded. Write the value itself.".into()),
            _ => {}
        }
    }
    let mut cmds = simple_commands(line);
    if cmds.len() != 1 {
        return Err("write one command: the test runner and its words".into());
    }
    let c = cmds.remove(0);
    let env = c.env.iter().filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    Ok((env, c.words))
}

/// `POST /api/tests/rerun {test, n, checkout?, command?, repo?}`:
/// Workbench runs the test N times itself, one after another, in a
/// checkout (the project's own, `main`, or an agent's worktree; by
/// default the one of the newest run), with the runner's command for that
/// one test or the command the user edited, and records each run like an
/// agent's (marked as Workbench's), on the commit it ran on.
/// `GET /api/tests/rerun/:id` follows it.
fn rerun_start(sup: &Arc<Supervisor>, b: RerunB) -> Result<Json, ApiError> {
    let n = b.n.unwrap_or(10).clamp(1, RERUN_MAX);
    let repo = repo_of(sup, b.repo.as_deref())?;
    let p = Params::new().set("repo", Value::Str(repo.clone())).set("test", Value::Str(b.test.clone()));
    let rows = rows_where(sup, "repo = :repo AND test = :test", &p, 100_000);
    let last = rows.last().ok_or_else(|| ApiError::not_found(format!("no runs of {} in {repo}", b.test)))?;
    let checkout = b.checkout.or(b.agent).filter(|a| !a.is_empty()).unwrap_or_else(|| if last.agent.is_empty() { MAIN_CHECKOUT.into() } else { last.agent.clone() });
    let (agent, dir, label) = if checkout == MAIN_CHECKOUT {
        (String::new(), repo.clone(), "the project's checkout".to_string())
    } else {
        let a = sup.agent_row(&checkout).ok_or_else(|| ApiError::not_found(format!("no agent {checkout}")))?;
        if a.repo != repo {
            return Err(ApiError::bad(format!("{} works in another project", a.name)));
        }
        let label = format!("{}'s worktree", a.name);
        (a.id, a.cwd, label)
    };
    if !Path::new(&dir).is_dir() {
        return Err(ApiError::bad(format!("{label} is gone ({dir})")));
    }
    let (env, words) = match b.command.as_deref().filter(|c| !c.trim().is_empty()) {
        Some(line) => typed_command(line).map_err(ApiError::bad)?,
        None => (
            Vec::new(),
            rerun_of(&rows, &b.test).ok_or_else(|| ApiError::bad(format!("Workbench cannot aim a {} command at one test: write the command yourself", if last.runner.is_empty() { "such" } else { last.runner.as_str() })))?,
        ),
    };
    if reruns().lock().unwrap().values().any(|r| r.state == "running" && r.test == b.test) {
        return Err(ApiError::bad(format!("{} is already being run", b.test)));
    }
    let id = crate::levels::new_id("rr");
    let (commit, tree) = code_of(Path::new(&dir));
    let r = Rerun {
        id: id.clone(),
        test: b.test.clone(),
        agent: agent.clone(),
        command: words.clone(),
        env: env.clone(),
        n,
        done: 0,
        passed: 0,
        failed: 0,
        state: "running",
        error: None,
        started_ms: now_ms(),
        commit,
        dirty: tree != "clean",
        branch: crate::git::current_branch(Path::new(&dir)),
        checkout: checkout.clone(),
        checkout_label: label,
    };
    let j = r.to_json();
    reruns().lock().unwrap().insert(id.clone(), r);
    let sup = sup.clone();
    let test = b.test;
    let place = Place { repo, cwd: dir };
    let _ = std::thread::Builder::new().name(format!("rerun-{id}")).spawn(move || {
        let envs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let cmd = if envs.is_empty() { shell_join(&words) } else { format!("{} {}", envs.join(" "), shell_join(&words)) };
        for i in 0..n {
            let t0 = now_ms();
            let res = run_once(&words, &env, Path::new(&place.cwd));
            let mut g = reruns().lock().unwrap();
            let Some(r) = g.get_mut(&id) else { return };
            if r.state == "stopped" {
                return;
            }
            match res {
                Ok((out, code)) => {
                    drop(g);
                    record_at(&sup, &agent, &place, Call { command: cmd.clone(), output: out.clone(), exit_code: code, use_id: Some(format!("{RERUN_PREFIX}{id}:{i}")), end_ms: now_ms().max(t0) });
                    let outcome = parse(&cmd, &out).tests.into_iter().find(|t| t.name == test).map(|t| t.outcome).unwrap_or(if code == 0 { "passed" } else { "failed" });
                    let mut g = reruns().lock().unwrap();
                    let Some(r) = g.get_mut(&id) else { return };
                    r.done += 1;
                    if outcome == "failed" {
                        r.failed += 1;
                    } else {
                        r.passed += 1;
                    }
                }
                Err(e) => {
                    r.state = "failed";
                    r.error = Some(e);
                    return;
                }
            }
        }
        if let Some(r) = reruns().lock().unwrap().get_mut(&id) {
            r.state = "done";
        }
    });
    Ok(json!({ "rerun": j }))
}

fn rerun_get(id: &str) -> Result<Json, ApiError> {
    reruns().lock().unwrap().get(id).map(|r| json!({ "rerun": r.to_json() })).ok_or_else(|| ApiError::not_found(format!("no run {id}")))
}

fn fmt_ms(ms: Option<i64>) -> String {
    match ms {
        None => "?".into(),
        Some(m) if m < 1000 => format!("{m} ms"),
        Some(m) if m < 60_000 => format!("{:.1} s", m as f64 / 1000.0),
        Some(m) => format!("{:.1} min", m as f64 / 60_000.0),
    }
}

/// `test_history`'s text for the radar: the flagged tests first.
pub fn radar_text(v: &Json) -> String {
    let t = &v["totals"];
    let mut out = vec![format!(
        "{}: {} tests, {} runs in {} commands over {} days; pass rate {:.0}%; {} flaky, {} newly failing, {} slow.",
        v["repo"].as_str().unwrap_or(""),
        t["tests"],
        t["runs"],
        t["commands"],
        v["window"]["days"],
        t["pass_rate"].as_f64().unwrap_or(1.0) * 100.0,
        t["flaky"],
        t["newly_failing"],
        t["slow"]
    )];
    let mut tests: Vec<&Json> = v["tests"].as_array().into_iter().flatten().collect();
    let rank = |x: &Json| (x["newly_failing"] == true) as u8 * 4 + (x["flaky"] == true) as u8 * 2 + (x["slow"] == true) as u8;
    tests.sort_by(|a, b| rank(b).cmp(&rank(a)).then(a["pass_rate"].as_f64().partial_cmp(&b["pass_rate"].as_f64()).unwrap_or(std::cmp::Ordering::Equal)));
    for x in tests.iter().take(40) {
        let mut flags = Vec::new();
        if x["newly_failing"] == true {
            flags.push("NEWLY FAILING".to_string());
        }
        if x["flaky"] == true {
            flags.push(format!("FLAKY ({})", x["flaky_why"].as_str().unwrap_or("")));
        }
        if x["slow"] == true {
            flags.push("SLOW".into());
        }
        out.push(format!(
            "{} [{}] {}/{} passed, p50 {}, p99 {}{}",
            x["test"].as_str().unwrap_or(""),
            x["runner"].as_str().unwrap_or(""),
            x["passed"],
            x["runs"],
            fmt_ms(x["p50_ms"].as_i64()),
            fmt_ms(x["p99_ms"].as_i64()),
            if flags.is_empty() { String::new() } else { format!("; {}", flags.join("; ")) }
        ));
    }
    if tests.len() > 40 {
        out.push(format!("… and {} more", tests.len() - 40));
    }
    out.join("\n")
}

/// `test_history`'s text for one test: its runs, newest first.
pub fn history_text(v: &Json) -> String {
    let mut out = vec![format!("{} [{}]", v["test"].as_str().unwrap_or(""), v["runner"].as_str().unwrap_or(""))];
    if v["flaky"] == true {
        out.push(format!("flaky: {}", v["flaky_why"].as_str().unwrap_or("")));
    }
    if v["newly_failing"] == true {
        out.push("newly failing: it passed before and fails now".into());
    }
    let names: HashMap<&str, &str> = v["agents"].as_array().into_iter().flatten().filter_map(|a| Some((a["id"].as_str()?, a["name"].as_str()?))).collect();
    for r in v["runs"].as_array().into_iter().flatten().take(30) {
        let agent = r["agent"].as_str().unwrap_or("");
        out.push(format!(
            "{} {} {} by {} on {}{}",
            r["ts"].as_str().unwrap_or(""),
            r["outcome"].as_str().unwrap_or(""),
            fmt_ms(r["ms"].as_i64()),
            names.get(agent).copied().unwrap_or(agent),
            r["commit"].as_str().map(|c| &c[..c.len().min(8)]).unwrap_or("no commit"),
            r["error"].as_str().map(|e| format!(": {}", one_line(e, 160))).unwrap_or_default()
        ));
    }
    out.join("\n")
}

fn repo_of(sup: &Supervisor, repo: Option<&str>) -> Result<String, ApiError> {
    match repo.filter(|r| !r.is_empty()) {
        Some(r) => Ok(crate::trail::repo_dir(Some(r)).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| r.to_string())),
        None => {
            // The repository most agents work in.
            let mut n: HashMap<String, usize> = HashMap::new();
            for a in sup.agent_rows() {
                *n.entry(a.repo).or_default() += 1;
            }
            n.into_iter().max_by_key(|(_, c)| *c).map(|(r, _)| r).ok_or_else(|| ApiError::bad("repo is required (no agent has run yet)"))
        }
    }
}

// ----------------------------------------------------------------- routes

#[derive(serde::Deserialize)]
struct TestsQ {
    repo: Option<String>,
    days: Option<i64>,
    test: Option<String>,
    agent: Option<String>,
}

async fn tests_route(State(app): State<App>, Query(q): Query<TestsQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || radar(&sup, q.repo.as_deref(), q.days.unwrap_or(7))).await?).into_response())
}

async fn history_route(State(app): State<App>, Query(q): Query<TestsQ>) -> ApiResult {
    let sup = app.sup.clone();
    let test = q.test.filter(|t| !t.is_empty()).ok_or_else(|| ApiError::bad("test is required"))?;
    Ok(AxJson(blocking(move || history(&sup, q.repo.as_deref(), &test)).await?).into_response())
}

async fn rerun_route(State(app): State<App>, body: axum::body::Bytes) -> ApiResult {
    let b: RerunB = crate::server::parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || rerun_start(&sup, b)).await?).into_response())
}

async fn rerun_get_route(axum::extract::Path(id): axum::extract::Path<String>) -> ApiResult {
    Ok(AxJson(rerun_get(&id)?).into_response())
}

async fn runs_route(State(app): State<App>, Query(q): Query<TestsQ>) -> ApiResult {
    let sup = app.sup.clone();
    let agent = q.agent.filter(|t| !t.is_empty()).ok_or_else(|| ApiError::bad("agent is required"))?;
    Ok(AxJson(blocking(move || agent_runs(&sup, &agent)).await?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_lines_report_times_and_failures() {
        let out = "running 3 tests\ntest totals::rounds ... ok <0.012s>\ntest totals::coupon ... FAILED <0.041s>\ntest totals::slow ... ignored\n\nfailures:\n\n---- totals::coupon stdout ----\nthread 'totals::coupon' panicked at tests/totals.rs:88:5:\nassertion `left == right` failed\n  left: 1799\n right: 1800\n\nfailures:\n    totals::coupon\n\ntest result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.52s\n";
        let p = parse("cargo test totals", out);
        assert_eq!(p.runner, "cargo");
        assert_eq!(p.tests.len(), 3);
        assert_eq!((p.tests[0].name.as_str(), p.tests[0].outcome, p.tests[0].ms), ("totals::rounds", "passed", Some(12)));
        assert_eq!((p.tests[1].outcome, p.tests[1].ms), ("failed", Some(41)));
        assert!(p.tests[1].error.as_deref().unwrap().contains("left: 1799"));
        assert_eq!(p.tests[2].outcome, "skipped");
        assert_eq!(p.total_ms, Some(520));
        let p = parse("cargo test", "test a::b ... ok\ntest a::c ... FAILED\n");
        assert_eq!(p.tests.iter().map(|t| t.ms).collect::<Vec<_>>(), vec![None, None]);
        let p = parse("cargo nextest run", "    PASS [   0.012s] shop tests::a\n    FAIL [   1.500s] shop tests::b\n");
        assert_eq!(p.runner, "nextest");
        assert_eq!((p.tests[1].name.as_str(), p.tests[1].outcome, p.tests[1].ms), ("shop tests::b", "failed", Some(1500)));
    }

    #[test]
    fn pytest_verbose_summary_and_durations() {
        let out = "tests/test_csv.py::test_bom PASSED                     [ 33%]\ntests/test_csv.py::test_sep FAILED                     [ 66%]\ntests/test_csv.py::test_skip SKIPPED (no db)           [100%]\n\n=================================== FAILURES ===================================\n___________________________________ test_sep ___________________________________\n\n    def test_sep():\n>       assert split('a;b') == ['a', 'b']\nE       AssertionError: assert ['a;b'] == ['a', 'b']\n\ntests/test_csv.py:12: AssertionError\n============================= slowest durations ==============================\n0.52s call     tests/test_csv.py::test_bom\n=========================== short test summary info ============================\nFAILED tests/test_csv.py::test_sep - AssertionError: assert ['a;b'] == ['a', 'b']\n==================== 1 failed, 1 passed, 1 skipped in 0.61s ====================\n";
        let p = parse("pytest tests -v --durations=5", out);
        assert_eq!(p.runner, "pytest");
        assert_eq!(p.tests.len(), 3);
        assert_eq!((p.tests[0].outcome, p.tests[0].ms), ("passed", Some(520)));
        assert_eq!(p.tests[1].outcome, "failed");
        assert!(p.tests[1].error.as_deref().unwrap().contains("AssertionError"));
        assert_eq!(p.tests[2].outcome, "skipped");
        assert_eq!(p.total_ms, Some(610));
        // Without -v: the failures from the summary, and its counts.
        let p = parse("python -m pytest -q", "..F.\nFAILED tests/a.py::test_x - assert 1 == 2\n1 failed, 3 passed in 0.10s\n");
        assert_eq!(p.tests.len(), 1);
        assert_eq!((p.tests[0].name.as_str(), p.tests[0].outcome), ("tests/a.py::test_x", "failed"));
        assert_eq!((p.counts, p.total_ms), (Some(Counts { passed: 3, failed: 1, skipped: 0 }), Some(100)));
        // A quiet pass: the count, and no test made up from the command.
        let p = parse("python3 -m pytest -q", "........                                     [100%]\n8 passed in 0.30s\n");
        assert_eq!((p.runner.as_str(), p.tests.len()), ("pytest", 0));
        assert_eq!((p.counts, p.total_ms), (Some(Counts { passed: 8, failed: 0, skipped: 0 }), Some(300)));
        // -rA names every test.
        let p = parse("pytest -q -rA", "...\n=== short test summary info ===\nPASSED test_coupons.py::test_a\nPASSED test_coupons.py::test_promo_window\nXFAIL test_coupons.py::test_trim - strict\n2 passed, 1 xfailed in 0.05s\n");
        assert_eq!(p.tests.iter().map(|t| (t.name.as_str(), t.outcome)).collect::<Vec<_>>(), [("test_coupons.py::test_a", "passed"), ("test_coupons.py::test_promo_window", "passed"), ("test_coupons.py::test_trim", "skipped")]);
        assert_eq!(p.counts.unwrap().total(), 3);
    }

    #[test]
    fn junit_reports_name_tests_as_pytest_does() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><testsuites><testsuite name="pytest" errors="0" failures="1" skipped="1" tests="4" time="0.312">
<testcase classname="test_coupons" file="test_coupons.py" line="3" name="test_welcome10" time="0.001" />
<testcase classname="test_coupons" file="test_coupons.py" line="9" name="test_promo_window" time="0.002"><failure message="assert 0.151 &gt; 0.2">def test_promo_window():
&gt;       assert random.random() &gt; 0.2
E       assert 0.151 &gt; 0.2</failure></testcase>
<testcase classname="tests.test_cart.TestCart" file="tests/test_cart.py" line="4" name="test_add[2-3]" time="0.010"></testcase>
<testcase classname="tests.test_cart.TestCart" file="tests/test_cart.py" line="8" name="test_skip" time="0.000"><skipped type="pytest.skip" message="later">later</skipped></testcase>
</testsuite></testsuites>"#;
        let p = parse_junit(xml, None);
        assert_eq!(p.total_ms, Some(312));
        let names: Vec<(&str, &str)> = p.tests.iter().map(|t| (t.name.as_str(), t.outcome)).collect();
        assert_eq!(names, [("test_coupons.py::test_welcome10", "passed"), ("test_coupons.py::test_promo_window", "failed"), ("tests/test_cart.py::TestCart::test_add[2-3]", "passed"), ("tests/test_cart.py::TestCart::test_skip", "skipped")]);
        assert!(p.tests[1].error.as_deref().unwrap().contains("assert 0.151 > 0.2"), "{:?}", p.tests[1].error);
        assert_eq!(p.tests[2].ms, Some(10));
        assert_eq!(p.counts, Some(Counts { passed: 2, failed: 1, skipped: 1 }));
        // xunit2 (no file): the class name read against the checkout.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/test_cart.py"), "").unwrap();
        let p = parse_junit(r#"<testsuite time="1"><testcase classname="tests.test_cart.TestCart" name="test_add" time="0.1"/></testsuite>"#, Some(dir.path()));
        assert_eq!(p.tests[0].name, "tests/test_cart.py::TestCart::test_add");
        let p = parse_junit(r#"<testcase classname="pkg.test_x" name="test_y"/>"#, None);
        assert_eq!(p.tests[0].name, "pkg/test_x.py::test_y");
        // The session's environment: the user's own PYTEST_ADDOPTS kept, and
        // none when the repository's config makes a junit report itself.
        let env = session_env(dir.path(), Path::new("/data/sessions/a1"));
        assert_eq!(env, [("PYTEST_ADDOPTS".to_string(), "--junitxml='/data/sessions/a1/pytest-junit.xml' -o junit_family=xunit1".to_string())]);
        std::fs::write(dir.path().join("pytest.ini"), "[pytest]\naddopts = --junitxml=out.xml\n").unwrap();
        assert!(session_env(dir.path(), Path::new("/data/sessions/a1")).is_empty());
        assert_eq!(junit_named("python3 -m pytest --junitxml=r/out.xml -q", Path::new("/w")), Some(Path::new("/w/r/out.xml").to_path_buf()));
        // A report belongs to the call that read it: the next call, even
        // one that starts within the time slack, does not read it again.
        let report = dir.path().join("report.xml");
        std::fs::write(&report, xml).unwrap();
        let now = now_ms();
        assert!(fresh_junit(&report, Some(now - 1000), now, dir.path(), Some("call-1")).is_some());
        assert!(fresh_junit(&report, Some(now - 1000), now, dir.path(), Some("call-1")).is_some(), "the same call, recorded again");
        assert!(fresh_junit(&report, Some(now), now + 10, dir.path(), Some("call-2")).is_none(), "the next call's report is its own");
    }

    #[test]
    fn one_test_again() {
        let w = |v: Option<Vec<String>>| v.map(|v| v.join(" "));
        assert_eq!(w(rerun_command("pytest", "cd shop && PYTHONPATH=. python3 -m pytest -q 2>&1 | tail -5", "test_coupons.py::test_promo_window")).as_deref(), Some("python3 -m pytest -q -rA -p no:cacheprovider test_coupons.py::test_promo_window"));
        assert_eq!(w(rerun_command("cargo", "cargo test -p calc --release totals", "totals::coupon")).as_deref(), Some("cargo test -p calc --release totals::coupon -- --exact"));
        assert_eq!(w(rerun_command("go", "go test ./... -v", "shop/pricing › TestFormat/eur")).as_deref(), Some("go test -count=1 -v -run ^TestFormat$ shop/pricing"));
        assert_eq!(rerun_command("jest", "npx jest", "a › b"), None);
        assert_eq!(rerun_command("pytest", "$(which pytest) -q", "a.py::t"), None);
        // A loop: the runner's invocation, never the shell's `do`.
        let looped = "D=/tmp/runs; for i in $(seq -w 2 15); do python3 -m pytest -v . > $D/run$i.log 2>&1; echo \"run$i exit=$? $(tail -1 $D/run$i.log)\"; done";
        assert_eq!(w(rerun_command("pytest", looped, "test_coupons.py::test_promo_window")).as_deref(), Some("python3 -m pytest -q -rA -p no:cacheprovider test_coupons.py::test_promo_window"));
        assert_eq!(w(rerun_command("pytest", "time python3 -m pytest -x tests/", "tests/a.py::t")).as_deref(), Some("python3 -m pytest -q -rA -p no:cacheprovider tests/a.py::t"));
        assert_eq!(w(rerun_command("pytest", "while true; do .venv/bin/pytest -q || break; done", "a.py::t")).as_deref(), Some(".venv/bin/pytest -q -rA -p no:cacheprovider a.py::t"));
        // The command as a user would edit it, and back.
        assert_eq!(shell_join(&["python3".into(), "-m".into(), "pytest".into(), "tests/test_cart.py::test_add[2-3]".into()]), "python3 -m pytest 'tests/test_cart.py::test_add[2-3]'");
        let (env, words) = typed_command("PLAYWRIGHT_MODULE=/x python3 -m pytest 'tests/test_cart.py::test_add[2-3]'").unwrap();
        assert_eq!((env, words.join(" ")), (vec![("PLAYWRIGHT_MODULE".to_string(), "/x".to_string())], "python3 -m pytest tests/test_cart.py::test_add[2-3]".to_string()));
        assert!(typed_command("for i in 1 2; do pytest; done").unwrap_err().contains("without a shell"));
        assert!(typed_command("pytest > log").is_err() && typed_command("pytest $X").is_err() && typed_command("  ").is_err());
    }

    #[test]
    fn shell_lines_split_into_simple_commands() {
        let c = simple_commands("cd shop && FOO=1 BAR='a b' time python3 -m pytest -q 2>&1 | tail -5; echo done");
        let words: Vec<String> = c.iter().map(|s| s.words.join(" ")).collect();
        assert_eq!(words, ["cd shop", "python3 -m pytest -q", "tail -5", "echo done"]);
        assert_eq!(c[1].env, ["FOO=1", "BAR=a b"]);
        let c = simple_commands("for i in $(seq 3); do pytest -q > \"$D/r$i.log\"; done");
        assert_eq!(c.iter().map(|s| s.words.join(" ")).collect::<Vec<_>>(), ["for i in $(seq 3)", "pytest -q"]);
        assert!(c[0].expands);
        assert_eq!(variant("PLAYWRIGHT_MODULE=/nonexistent python3 -m pytest -q test_checkout_page.py 2>&1 | tail -2"), "PLAYWRIGHT_MODULE=/nonexistent");
        assert_eq!(variant("cd x && python3 -m pytest -q"), "");
    }

    #[test]
    fn a_loop_of_runs_is_that_many_runs() {
        // Sam's loop: each run's output to a log, its last line echoed.
        let mut out = String::new();
        for i in 2..=15 {
            out += &format!("run{i:02} exit=0 ============================== 8 passed in 0.02s ===============================\n");
        }
        let segs = invocations("for i in $(seq -w 2 15); do python3 -m pytest -v . > $D/run$i.log 2>&1; echo \"run$i exit=$? $(tail -1 $D/run$i.log)\"; done", &out);
        assert_eq!(segs.len(), 14);
        // The junit report is the last run's: it names the tests, and the
        // other runs (8 passed, nothing else) get the same names.
        let names = ["test_a.py::t1", "test_a.py::t2", "test_b.py::t3", "test_b.py::t4", "test_c.py::t5", "test_c.py::t6", "test_c.py::t7", "test_c.py::t8"];
        let junit = Parsed { runner: "junit".into(), tests: names.iter().map(|n| TestResult { name: n.to_string(), outcome: "passed", ms: Some(1), error: None }).collect(), total_ms: Some(20), counts: Some(Counts { passed: 8, failed: 0, skipped: 0 }) };
        let pieces = pieces_of("for i in 1 2; do python3 -m pytest -v .; done", &out, 0, Some(junit));
        assert_eq!(pieces.len(), 14);
        assert!(pieces.iter().all(|p| p.tests.len() == 8 && p.counts == Some(Counts { passed: 8, failed: 0, skipped: 0 })), "{pieces:?}");
        assert_eq!((pieces[0].source, pieces[13].source), ("inferred", "junit"));
        // -v runs in a row, one failing: each run its own tests.
        let v = "test_coupons.py::test_pct PASSED [ 50%]\ntest_coupons.py::test_promo_window PASSED [100%]\n==== 2 passed in 0.01s ====\ntest_coupons.py::test_pct PASSED [ 50%]\ntest_coupons.py::test_promo_window FAILED [100%]\n___ test_promo_window ___\nE   assert 0.08 > 0.2\n=== short test summary info ===\nFAILED test_coupons.py::test_promo_window - assert 0.08 > 0.2\n==== 1 failed, 1 passed in 0.02s ====\n";
        let pieces = pieces_of("for i in 1 2; do pytest -v test_coupons.py; done", v, 1, None);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].tests.iter().map(|t| t.outcome).collect::<Vec<_>>(), ["passed", "passed"]);
        assert_eq!((pieces[1].tests[1].outcome, pieces[1].exit_code, pieces[0].exit_code), ("failed", 1, 0));
        assert!(pieces[1].tests[1].error.as_deref().unwrap().contains("0.08 > 0.2"));
        // -q runs that only count, one failure named: inferred from the named run.
        let q = "1 failed, 7 passed in 0.03s\n8 passed in 0.02s\n";
        let pieces = pieces_of("for i in 1 2; do pytest -q; done", q, 1, None);
        assert_eq!(pieces.iter().map(|p| p.tests.len()).collect::<Vec<_>>(), [0, 0], "no run named its tests: counts only");
        // One run: one piece, the call's exit code.
        assert_eq!(pieces_of("pytest -q", "8 passed in 0.02s\n", 0, None).len(), 1);
        // cargo twice in a row: two runs.
        let cargo = "   Compiling x v0.1.0\n    Finished `test` profile [unoptimized] target(s) in 1.00s\n     Running unittests\ntest a::b ... ok\ntest result: ok. 1 passed; 0 failed\n    Finished `test` profile [unoptimized] target(s) in 0.05s\n     Running unittests\ntest a::b ... FAILED\ntest result: FAILED. 0 passed; 1 failed\n";
        let pieces = pieces_of("cargo test && cargo test", cargo, 101, None);
        assert_eq!(pieces.iter().map(|p| p.tests[0].outcome).collect::<Vec<_>>(), ["passed", "failed"]);
        // An echo's prefix before pytest's summary.
        assert_eq!(pytest_summary_in("run03 exit=1 ===== 1 failed, 7 passed in 0.04s =====").map(|c| c.0), Some(Counts { passed: 7, failed: 1, skipped: 0 }));
        assert_eq!(pytest_summary_in("I ran it in a loop and 8 passed"), None);
    }

    fn row(outcome: &str, commit: &str, tree: &str, ts: i64) -> Row {
        Row { test: "t".into(), run: format!("r{ts}"), agent: "a".into(), commit: Some(commit.into()), tree: tree.into(), outcome: outcome.into(), ms: Some(10), own_ms: true, ts, runner: "cargo".into(), command: "cargo test".into(), exit_code: Some(0), seq: None, error: None, use_id: None, wait_ms: None }
    }

    #[test]
    fn verdicts() {
        // Same commit, same code, both outcomes: flaky.
        let rs = [row("passed", "c1", "clean", 1), row("failed", "c1", "clean", 2), row("passed", "c1", "clean", 3)];
        let v = verdict(&rs.iter().collect::<Vec<_>>());
        assert!(v.flaky && v.flaky_why.unwrap().contains("same code: 1 of 3 runs failed"));
        assert!(!v.newly_failing);
        // Code changed between them, one flip: an agent broke it; newly failing.
        let rs = [row("passed", "c1", "clean", 1), row("failed", "c1", "d1", 2)];
        let v = verdict(&rs.iter().collect::<Vec<_>>());
        assert!(!v.flaky && v.newly_failing);
        // Failed, then passed after the code changed (another agent's
        // failure, never on the same code twice): a fix, not a flake.
        let mut rs = [row("passed", "c1", "a", 1), row("failed", "c1", "b", 2), row("passed", "c1", "c", 3)];
        rs[1].agent = "b".into();
        assert!(!verdict(&rs.iter().collect::<Vec<_>>()).flaky);
        // A run with an environment of its own (a skip check, a broken
        // dependency on purpose) is not compared with the normal runs.
        let mut rs = [row("passed", "c1", "clean", 1), row("failed", "c1", "clean", 2)];
        rs[1].command = "PLAYWRIGHT_MODULE=/nonexistent cargo test".into();
        let v = verdict(&rs.iter().collect::<Vec<_>>());
        assert!(!v.flaky, "{v:?}");
        assert_eq!(v.stable.as_ref().map(|s| s["runs"].as_i64()), Some(Some(1)), "its last normal run decides: {v:?}");
        // Fail before fix: the agent's test failed, it edited, it passed.
        // Not flaky, and the failure is not held against the pass rate.
        let rs = [row("passed", "c1", "a", 1), row("failed", "c1", "b", 2), row("passed", "c1", "c", 3), row("passed", "c1", "c", 4)];
        let v = verdict(&rs.iter().collect::<Vec<_>>());
        assert!(!v.flaky && v.fixed == 1, "{v:?}");
        let (j, _) = test_json("t", &rs.iter().collect::<Vec<_>>(), None);
        assert_eq!((j["pass_rate"].as_f64(), j["fixed"].as_i64()), (Some(1.0), Some(1)));
        assert_eq!((j["stable"]["runs"].as_i64(), j["stable"]["since_tree"].as_str()), (Some(2), Some("c")), "{j}");
        // A flaky test, then fifteen passes after a fix: stable on the
        // fix's commit, its runs only, against the commit it failed on.
        let mut rs: Vec<Row> = (0..10).map(|i| row(if i % 5 == 4 { "failed" } else { "passed" }, "c1", "clean", i)).collect();
        rs.extend((10..25).map(|i| row("passed", "c2", "clean", i)));
        let v = verdict(&rs.iter().collect::<Vec<_>>());
        assert!(v.flaky);
        let st = v.stable.unwrap();
        assert_eq!((st["since_commit"].as_str(), st["runs"].as_i64(), st["failed_before"].as_i64(), st["runs_before"].as_i64(), st["before_commit"].as_str()), (Some("c2"), Some(15), Some(2), Some(10), Some("c1")), "{st}");
        // Failing 2 in 10, a test passes 14 times in a row under 5% of the time.
        assert_eq!((st["needed"].as_i64(), st["enough"].as_bool()), (Some(14), Some(true)), "{st}");
        assert!((st["upper_95"].as_f64().unwrap() - 0.181).abs() < 0.001, "{st}");
        assert_eq!(v.codes.len(), 2);
        assert_eq!((v.codes[0]["commit"].as_str(), v.codes[0]["runs"].as_i64(), v.codes[1]["failed"].as_i64()), (Some("c2"), Some(15), Some(2)));
        // The broken commit's runs are never the fixed commit's: Sam's case,
        // the old code still failing after the fix (on main), the fix's
        // runs after that.
        let mut rs: Vec<Row> = vec![row("failed", "c1", "clean", 1)];
        rs.extend((2..16).map(|i| row("passed", "c1", "clean", i)));
        rs.extend((16..26).map(|i| row("passed", "c2", "clean", i)));
        rs.push(row("failed", "c1", "clean", 26));
        rs.extend((27..37).map(|i| row("passed", "c2", "clean", i)));
        let v = verdict(&rs.iter().collect::<Vec<_>>());
        assert!(v.flaky_why.as_deref().unwrap().contains("c1 with the same code: 2 of 16 runs failed"), "{v:?}");
        let st = v.stable.unwrap();
        assert_eq!((st["since_commit"].as_str(), st["runs"].as_i64(), st["runs_before"].as_i64()), (Some("c2"), Some(20), Some(16)));
        assert_eq!((st["needed"].as_i64(), st["enough"].as_bool()), (Some(23), Some(false)), "not enough runs yet: {st}");
        // Too few runs to say anything: no number.
        assert_eq!(passes_needed(0, 0), 0);
        assert_eq!(passes_needed(1, 1), 1);
        // A new commit between them: not.
        let rs = [row("passed", "c1", "a", 1), row("failed", "c2", "b", 2), row("passed", "c3", "c", 3)];
        assert!(!verdict(&rs.iter().collect::<Vec<_>>()).flaky);
        assert_eq!(percentile(&[1, 2, 3, 4], 0.5), Some(2));
        assert_eq!(percentile(&[1, 2, 3, 4], 0.99), Some(4));
        assert!(is_slow(Some(100), Some(6000)) && is_slow(Some(300), Some(1200)) && !is_slow(Some(400), Some(900)));
    }
}
