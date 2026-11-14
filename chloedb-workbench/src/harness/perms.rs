//! The permissions part of the harness (HARNESS.md, "Editor: Permissions",
//! "Lint", "Impact preview", "Safety"): Claude Code's rules as Workbench
//! reads them, the tool calls agents made with whether they were prompted
//! and what the user answered, and from those two, exact (counted) answers:
//! the hits of each rule, the lint, the impact of a change on the last days,
//! and whether a version widens what agents may do.
//!
//! Matching follows Claude Code's rules as Workbench writes them:
//! `Bash(prefix:*)` matches a simple command that starts with `prefix` as
//! typed (a leading `VAR=value` is part of what is typed, which is why
//! `Bash(python3 -m pytest:*)` never matches `PYTHONPATH=. python3 -m
//! pytest`), `Bash(cmd)` the command exactly; `Edit(//abs/**)` and
//! `Read(…)` a file path by glob (`Edit` covers Write, MultiEdit and
//! NotebookEdit, `Read` covers Grep and Glob); a bare tool name or
//! `mcp__server` every call of it. A command of several simple commands
//! (`&&`, `||`, `;`, `|`) is allowed when each is; deny wins over ask, ask
//! over allow, and a call no rule allows is prompted.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::db::Event;

/// Where a version's own rules go: what it adds to the rules Workbench
/// writes for every session, and which of those it takes away.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermEdits {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
    /// Rules of Workbench's own (the baseline) this version leaves out.
    pub remove: Vec<String>,
}

impl PermEdits {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.ask.is_empty() && self.deny.is_empty() && self.remove.is_empty()
    }
}

/// One rule with where it came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Rule {
    pub decision: String,
    pub text: String,
    /// `Workbench`, `Flow`, or the version that added it.
    pub from: String,
}

/// The whole set a session gets: Workbench's rules minus what a version
/// removes, plus what it adds.
pub fn effective(baseline: &[Rule], edits: &PermEdits, by: &str) -> Vec<Rule> {
    let mut out: Vec<Rule> = baseline.iter().filter(|r| !edits.remove.contains(&r.text)).cloned().collect();
    for (d, list) in [("deny", &edits.deny), ("ask", &edits.ask), ("allow", &edits.allow)] {
        for t in list {
            if !out.iter().any(|r| r.decision == d && &r.text == t) {
                out.push(Rule { decision: d.into(), text: t.clone(), from: by.into() });
            }
        }
    }
    out
}

/// A settings file's `permissions` block of `rules`, as Workbench writes it.
pub fn block(rules: &[Rule]) -> Json {
    let of = |d: &str| rules.iter().filter(|r| r.decision == d).map(|r| r.text.clone()).collect::<Vec<_>>();
    let mut b = serde_json::Map::new();
    for d in ["deny", "ask", "allow"] {
        let l = of(d);
        if !l.is_empty() {
            b.insert(d.into(), json!(l));
        }
    }
    Json::Object(b)
}

/// The edits that make `baseline` into the rules of a `permissions`
/// block (the raw settings JSON parsed back).
pub fn edits_of_block(baseline: &[Rule], block: &Json) -> Result<PermEdits, String> {
    let obj = block.as_object().ok_or("permissions is not an object")?;
    for k in obj.keys() {
        if !matches!(k.as_str(), "allow" | "ask" | "deny" | "defaultMode" | "additionalDirectories") {
            return Err(format!("permissions.{k} is not a key Claude Code knows"));
        }
    }
    let list = |d: &str| -> Result<Vec<String>, String> {
        match obj.get(d) {
            None => Ok(Vec::new()),
            Some(Json::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| format!("permissions.{d} holds something that is not a string"))).collect(),
            Some(_) => Err(format!("permissions.{d} is not a list")),
        }
    };
    let mut e = PermEdits::default();
    let mut want: Vec<(String, String)> = Vec::new();
    for d in ["deny", "ask", "allow"] {
        for t in list(d)? {
            if t.trim().is_empty() {
                return Err(format!("an empty rule in permissions.{d}"));
            }
            want.push((d.to_string(), t));
        }
    }
    for r in baseline {
        if !want.iter().any(|(d, t)| *d == r.decision && *t == r.text) {
            e.remove.push(r.text.clone());
        }
    }
    for (d, t) in want {
        if baseline.iter().any(|r| r.decision == d && r.text == t) {
            continue;
        }
        match d.as_str() {
            "deny" => e.deny.push(t),
            "ask" => e.ask.push(t),
            _ => e.allow.push(t),
        }
    }
    Ok(e)
}

// ------------------------------------------------------------- matching

/// A rule's tool and its specifier: `Bash(git push:*)` is (`Bash`, `git push:*`).
pub fn parse(rule: &str) -> (String, Option<String>) {
    let r = rule.trim();
    match (r.find('('), r.ends_with(')')) {
        (Some(i), true) => (r[..i].to_string(), Some(r[i + 1..r.len() - 1].to_string())),
        _ => (r.to_string(), None),
    }
}

/// The simple commands of a shell command, as typed (split at `&&`, `||`,
/// `;`, `|` and new lines).
pub fn parts(cmd: &str) -> Vec<String> {
    cmd.split("&&")
        .flat_map(|p| p.split("||"))
        .flat_map(|p| p.split([';', '|', '\n']))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// A simple command without its leading `VAR=value` words: what a rule
/// written from the command's name expects.
pub fn bare(part: &str) -> String {
    let words: Vec<&str> = part.split_whitespace().collect();
    let skip = words.iter().take_while(|w| w.contains('=') && !w.starts_with('=') && !w.starts_with('-')).count();
    words[skip..].join(" ")
}

fn tool_covers(rule_tool: &str, tool: &str) -> bool {
    if rule_tool == tool {
        return true;
    }
    if rule_tool.starts_with("mcp__") {
        return tool.starts_with(&format!("{rule_tool}__"));
    }
    match rule_tool {
        "Edit" => matches!(tool, "Write" | "MultiEdit" | "NotebookEdit"),
        "Read" => matches!(tool, "Grep" | "Glob" | "LS"),
        _ => false,
    }
}

/// Whether a Bash specifier matches one simple command.
fn bash_spec(spec: &str, part: &str) -> bool {
    let spec = spec.trim();
    if spec == "*" {
        return true;
    }
    match spec.strip_suffix(":*").or_else(|| spec.strip_suffix(" *")) {
        Some(prefix) => part == prefix || part.strip_prefix(prefix).is_some_and(|rest| rest.starts_with(' ') || prefix.ends_with(' ')),
        None => part == spec,
    }
}

/// `*` within a name, `**` across folders.
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') if p.get(1) == Some(&b'*') => {
            let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
            (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| glob(&p[1..], &s[i..])),
        Some(c) => s.first() == Some(c) && glob(&p[1..], &s[1..]),
    }
}

/// Whether a path specifier (`//abs/**`, `/from-the-project/**`, `src/**`,
/// `~/x`) matches a file of a session in `cwd`.
fn path_spec(spec: &str, file: &str, cwd: &str) -> bool {
    let pat = if let Some(abs) = spec.strip_prefix("//") {
        format!("/{abs}")
    } else if let Some(rel) = spec.strip_prefix('/') {
        format!("{}/{rel}", cwd.trim_end_matches('/'))
    } else if let Some(home) = spec.strip_prefix("~/") {
        format!("{}/{home}", std::env::var("HOME").unwrap_or_default())
    } else {
        format!("{}/{}", cwd.trim_end_matches('/'), spec.trim_start_matches("./"))
    };
    glob(pat.as_bytes(), file.as_bytes()) || pat.strip_suffix("/**").is_some_and(|dir| file == dir)
}

/// A tool call as the rules see it.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub tool: String,
    pub command: Option<String>,
    pub path: Option<String>,
    pub cwd: String,
}

/// `<worktree>` in a rule, made the session's folder.
fn expand(rule: &str, cwd: &str) -> String {
    rule.replace("<worktree>", cwd)
}

/// Whether `rule` matches `part` (a simple command, or the whole call when
/// it is not Bash).
fn rule_matches(rule: &str, p: &Probe, part: Option<&str>) -> bool {
    let rule = expand(rule, &p.cwd);
    let (tool, spec) = parse(&rule);
    if !tool_covers(&tool, &p.tool) {
        return false;
    }
    let Some(spec) = spec else { return true };
    if p.tool == "Bash" {
        return part.is_some_and(|c| bash_spec(&spec, c));
    }
    if let Some(domain) = spec.strip_prefix("domain:") {
        return p.path.as_deref().is_some_and(|u| u.split("://").nth(1).unwrap_or(u).split(['/', ':']).next() == Some(domain));
    }
    p.path.as_deref().is_some_and(|f| path_spec(&spec, f, &p.cwd))
}

/// The decision rank: what may run without asking is the widest.
pub fn rank(d: &str) -> u8 {
    match d {
        "deny" => 0,
        "ask" | "prompt" => 1,
        _ => 2,
    }
}

/// What the rules decide for a call: `deny`, `ask`, `allow`, or `prompt`
/// (no rule allows it, so Claude Code asks).
pub fn decide(rules: &[Rule], p: &Probe) -> &'static str {
    let ps: Vec<Option<String>> = match (&p.command, p.tool.as_str()) {
        (Some(c), "Bash") => {
            let v = parts(c);
            if v.is_empty() {
                vec![Some(String::new())]
            } else {
                v.into_iter().map(Some).collect()
            }
        }
        _ => vec![None],
    };
    let of = |d: &'static str| rules.iter().filter(move |r| r.decision == d);
    if ps.iter().any(|part| of("deny").any(|r| rule_matches(&r.text, p, part.as_deref()))) {
        return "deny";
    }
    if ps.iter().any(|part| of("ask").any(|r| rule_matches(&r.text, p, part.as_deref()))) {
        return "ask";
    }
    let allowed = |part: &Option<String>| part.as_deref().is_some_and(|c| c.starts_with("cd ") || c == "cd") || of("allow").any(|r| rule_matches(&r.text, p, part.as_deref()));
    if ps.iter().all(allowed) {
        "allow"
    } else {
        "prompt"
    }
}

/// Whether `rule` decided `p` (any of its simple commands).
pub fn hits(rule: &str, p: &Probe) -> bool {
    match (&p.command, p.tool.as_str()) {
        (Some(c), "Bash") => parts(c).iter().any(|part| rule_matches(rule, p, Some(part))),
        _ => rule_matches(rule, p, None),
    }
}

// ---------------------------------------------------------------- calls

/// One tool call of an agent, from its hooks: whether Claude Code asked,
/// what the user answered, and how long the prompt waited.
#[derive(Clone, Debug)]
pub struct Call {
    pub agent: String,
    pub name: String,
    pub ts: i64,
    pub probe: Probe,
    pub prompted: bool,
    /// `ran`, `refused` (prompted and never ran), `blocked` (auto mode's
    /// classifier denied it: a `PermissionDenied` event), or `open`.
    pub outcome: &'static str,
    pub wait_ms: i64,
    /// The permission mode the session was in (`permission_mode` of the
    /// hook's input: `default`, `auto`, `acceptEdits`, `plan`,
    /// `bypassPermissions`), or `unknown` when no event said.
    pub mode: String,
}

fn input_path(d: &Json) -> Option<String> {
    let i = &d["tool_input"];
    i["file_path"].as_str().or(i["notebook_path"].as_str()).or(i["path"].as_str()).or(i["url"].as_str()).map(str::to_string)
}

fn is_permission_event(ev: &Event, d: &Json) -> bool {
    match d.get("notification_type").and_then(Json::as_str) {
        Some(t) => t.contains("permission"),
        None => ev.summary.as_deref().or(d["message"].as_str()).is_some_and(|m| m.to_ascii_lowercase().contains("permission")),
    }
}

/// The tool calls of one agent, in order.
pub fn calls_of(agent: &str, name: &str, cwd: &str, evs: &[Event]) -> Vec<Call> {
    let mut out: Vec<Call> = Vec::new();
    // Open calls: (index in out, use id, input text).
    let mut open: Vec<(usize, Option<String>, String)> = Vec::new();
    let mut asked_at: Option<(usize, i64)> = None;
    let mut mode = String::from("unknown");
    let close_open = |out: &mut Vec<Call>, open: &mut Vec<(usize, Option<String>, String)>| {
        for (i, _, _) in open.drain(..) {
            out[i].outcome = if out[i].prompted { "refused" } else { "open" };
        }
    };
    for ev in evs {
        let d: Json = ev.data.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Json::Null);
        if let Some(m) = d["permission_mode"].as_str().filter(|m| !m.is_empty() && m.len() < 40) {
            mode = m.to_string();
        }
        // The first event after a prompt answers it.
        if let Some((i, t)) = asked_at {
            if ev.kind != "Notification" {
                out[i].wait_ms = (ev.ts - t).max(0);
                asked_at = None;
            }
        }
        match ev.kind.as_str() {
            "PreToolUse" => {
                let tool = ev.tool.clone().unwrap_or_default();
                let command = d["tool_input"]["command"].as_str().map(str::to_string);
                out.push(Call {
                    agent: agent.into(),
                    name: name.into(),
                    ts: ev.ts,
                    probe: Probe { tool, command, path: input_path(&d), cwd: cwd.into() },
                    prompted: false,
                    outcome: "open",
                    wait_ms: 0,
                    mode: mode.clone(),
                });
                open.push((out.len() - 1, d["tool_use_id"].as_str().map(str::to_string), d["tool_input"].to_string()));
            }
            "Notification" if is_permission_event(ev, &d) => {
                if let Some((i, _, _)) = open.last() {
                    out[*i].prompted = true;
                    asked_at = Some((*i, ev.ts));
                }
            }
            "PostToolUse" | "PostToolUseFailure" => {
                let tool = ev.tool.clone().unwrap_or_default();
                let id = d["tool_use_id"].as_str();
                let input = d["tool_input"].to_string();
                let at = open
                    .iter()
                    .rposition(|(_, u, _)| id.is_some() && u.as_deref() == id)
                    .or_else(|| open.iter().rposition(|(i, _, inp)| out[*i].probe.tool == tool && *inp == input))
                    .or_else(|| open.iter().rposition(|(i, _, _)| out[*i].probe.tool == tool));
                if let Some(k) = at {
                    let (i, _, _) = open.remove(k);
                    out[i].outcome = "ran";
                }
            }
            // Auto mode's classifier said no (Claude Code's PermissionDenied
            // hook, when a session wires it).
            "PermissionDenied" => {
                let id = d["tool_use_id"].as_str();
                let tool = ev.tool.clone().unwrap_or_default();
                let at = open.iter().rposition(|(_, u, _)| id.is_some() && u.as_deref() == id).or_else(|| open.iter().rposition(|(i, _, _)| out[*i].probe.tool == tool));
                if let Some(k) = at {
                    let (i, _, _) = open.remove(k);
                    out[i].outcome = "blocked";
                    out[i].mode = "auto".into();
                }
            }
            "UserPromptSubmit" | "Stop" | "SessionEnd" | "exited" => close_open(&mut out, &mut open),
            _ => {}
        }
    }
    close_open(&mut out, &mut open);
    out
}

// ------------------------------------------------------------ the lattice

/// Calls (and probes made from the rules' own patterns) that `after`
/// decides more widely than `before`: each as `{what, before, after}`.
/// It is computed over the whole version, so two rules that together allow
/// what neither does alone are caught.
pub fn widening(before: &[Rule], after: &[Rule], calls: &[Call], cwd: &str) -> Vec<Json> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let mut check = |p: &Probe, what: String, out: &mut Vec<Json>| {
        if !seen.insert(what.clone()) {
            return;
        }
        let (a, b) = (decide(before, p), decide(after, p));
        if rank(b) > rank(a) {
            out.push(json!({ "what": what, "before": a, "after": b }));
        }
    };
    for c in calls {
        let what = c.probe.command.clone().or_else(|| c.probe.path.clone()).map(|w| format!("{}: {w}", c.probe.tool)).unwrap_or_else(|| c.probe.tool.clone());
        check(&c.probe, what, &mut out);
    }
    // The rules' own patterns, as calls they would allow.
    for r in after.iter().filter(|r| r.decision == "allow") {
        let (tool, spec) = parse(&expand(&r.text, cwd));
        let p = match (tool.as_str(), &spec) {
            ("Bash", Some(s)) => Probe { tool: tool.clone(), command: Some(s.trim_end_matches(":*").trim_end_matches(" *").to_string()), path: None, cwd: cwd.into() },
            (_, Some(s)) => Probe { tool: tool.clone(), command: None, path: Some(s.replace("**", "x").replace('*', "x").trim_start_matches('/').to_string()).map(|p| format!("/{p}")), cwd: cwd.into() },
            (_, None) => Probe { tool: tool.clone(), command: None, path: None, cwd: cwd.into() },
        };
        check(&p, format!("rule {}", r.text), &mut out);
    }
    out
}

// ------------------------------------------------------------------ lint

static SECRET_PATTERNS: [(&str, &str); 9] = [
    ("sk-ant-", "an Anthropic key"),
    ("sk_live_", "a Stripe live key"),
    ("sk_test_", "a Stripe test key"),
    ("ghp_", "a GitHub token"),
    ("github_pat_", "a GitHub token"),
    ("xoxb-", "a Slack token"),
    ("AKIA", "an AWS key"),
    ("-----BEGIN", "a private key"),
    ("sk-", "an API key"),
];

/// Secrets in a part's text: `{rule: "secret", range: [from, to], message}`.
pub fn secrets(text: &str) -> Vec<Json> {
    let mut out = Vec::new();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    for (pat, what) in SECRET_PATTERNS {
        let mut from = 0;
        while let Some(i) = text[from..].find(pat) {
            let at = from + i;
            from = at + pat.len();
            let end = at + text[at..].find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ')' | '}')).unwrap_or(text.len() - at);
            if end - at < pat.len() + 8 || taken.iter().any(|(a, b)| at < *b && end > *a) {
                continue;
            }
            if pat == "AKIA" && !text[at..end].chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
                continue;
            }
            taken.push((at, end));
            let shown: String = text[at..end].chars().take(pat.len() + 3).collect();
            out.push(json!({ "rule": "secret", "range": [at, end], "severity": "bad", "message": format!("{what} in the harness ({shown}…): every agent gets it. Move it to the environment of the program that needs it.") }));
        }
    }
    // `KEY=value` or `"KEY": "value"` for names that say secret.
    for (i, line) in text.lines().enumerate() {
        let l = line.trim();
        let upper = l.to_ascii_uppercase();
        let named = ["_KEY", "_TOKEN", "_SECRET", "PASSWORD"].iter().any(|n| upper.split(['=', ':']).next().is_some_and(|k| k.trim_matches(['"', ' ']).ends_with(n)));
        let value = l.split_once(['=', ':']).map(|(_, v)| v.trim().trim_matches(['"', ',', ' ']).to_string()).unwrap_or_default();
        if named && value.len() >= 8 && !value.starts_with('$') && !out.iter().any(|o| o["line"] == json!(i)) {
            let at = text.lines().take(i).map(|x| x.len() + 1).sum::<usize>();
            if taken.iter().any(|(a, b)| *a >= at && *b <= at + line.len()) {
                continue;
            }
            out.push(json!({ "rule": "secret", "range": [at, at + line.len()], "line": i, "severity": "bad", "message": format!("{} looks like a secret passed to every agent. Move it to the environment of the program that needs it.", l.split(['=', ':']).next().unwrap_or("").trim().trim_matches('"')) }));
        }
    }
    out
}

/// A prefix rule's words, if it is one.
fn prefix_of(rule: &str) -> Option<String> {
    let (tool, spec) = parse(rule);
    if tool != "Bash" {
        return None;
    }
    let s = spec?;
    s.strip_suffix(":*").or_else(|| s.strip_suffix(" *")).map(|p| p.trim().to_string())
}

/// Lint of a set of rules against the calls of the window. Each flag is
/// `{rule, pattern, severity, message, fix?}`.
pub fn lint(rules: &[Rule], current: &[Rule], calls: &[Call], cwd: &str) -> Vec<Json> {
    let mut out = Vec::new();
    let count_hits = |rule: &str| calls.iter().filter(|c| hits(rule, &c.probe)).count();
    // Prompted commands by their simple command without VAR= words.
    let prompted: Vec<&Call> = calls.iter().filter(|c| c.prompted && c.probe.tool == "Bash").collect();
    for r in rules {
        let n = count_hits(&r.text);
        if r.decision == "allow" {
            if let Some(prefix) = prefix_of(&r.text) {
                // Unreachable: never matched, while commands that would
                // match without their VAR= words were asked for.
                if n == 0 {
                    let typed: Vec<&&Call> = prompted
                        .iter()
                        .filter(|c| c.probe.command.as_deref().is_some_and(|cmd| parts(cmd).iter().any(|p| *p != bare(p) && bash_spec(&format!("{prefix}:*"), &bare(p)))))
                        .collect();
                    if let Some(first) = typed.first() {
                        let ex = parts(first.probe.command.as_deref().unwrap_or("")).into_iter().find(|p| *p != bare(p)).unwrap_or_default();
                        let as_typed = ex.split_whitespace().take(prefix.split_whitespace().count() + ex.split_whitespace().take_while(|w| w.contains('=')).count()).collect::<Vec<_>>().join(" ");
                        out.push(json!({
                            "rule": "unreachable", "pattern": r.text, "severity": "warn",
                            "message": format!("Never matches: agents type `{as_typed}`, and a rule matches from the first word typed. {} prompt{} for it in the window.", typed.len(), if typed.len() == 1 { "" } else { "s" }),
                            "fix": format!("Bash({as_typed}:*)"),
                        }));
                        continue;
                    }
                }
                // Wider than needed: a one-word prefix (an interpreter, a
                // package manager) whose hits all start the same few ways.
                if prefix.split_whitespace().count() == 1 && n > 0 {
                    let mut ways: BTreeMap<String, usize> = BTreeMap::new();
                    for c in calls.iter().filter(|c| hits(&r.text, &c.probe)) {
                        for p in parts(c.probe.command.as_deref().unwrap_or("")) {
                            if bash_spec(&r.text[5..r.text.len() - 1], &p) {
                                let two = p.split_whitespace().take(if p.split_whitespace().nth(1).is_some_and(|w| w == "-m") { 3 } else { 2 }).collect::<Vec<_>>().join(" ");
                                *ways.entry(two).or_default() += 1;
                            }
                        }
                    }
                    if !ways.is_empty() && ways.len() <= 3 {
                        let list: Vec<String> = ways.iter().map(|(w, k)| format!("`{w}` ({k})")).collect();
                        out.push(json!({
                            "rule": "wide", "pattern": r.text, "severity": "warn",
                            "message": format!("Wider than needed: {n} hits, all of them {}. It also allows any other `{prefix}` command.", list.join(", ")),
                            "fix": ways.keys().map(|w| format!("Bash({w}:*)")).collect::<Vec<_>>(),
                        }));
                    }
                }
                if matches!(prefix.as_str(), "" | "*") {
                    out.push(json!({ "rule": "wide", "pattern": r.text, "severity": "bad", "message": "Allows every shell command." }));
                }
            } else if r.text == "Bash" || r.text == "Bash(*)" {
                out.push(json!({ "rule": "wide", "pattern": r.text, "severity": "bad", "message": "Allows every shell command." }));
            }
        }
        // Conflict: an allow and a deny or ask that decide the same calls.
        if r.decision == "allow" {
            for other in rules.iter().filter(|o| o.decision != "allow") {
                let both = calls.iter().filter(|c| hits(&r.text, &c.probe) && hits(&other.text, &c.probe)).count();
                let same_prefix = prefix_of(&r.text).zip(prefix_of(&other.text)).is_some_and(|(a, b)| a.starts_with(&b) || b.starts_with(&a));
                if both > 0 || same_prefix {
                    out.push(json!({
                        "rule": "conflict", "pattern": r.text, "severity": "warn",
                        "message": format!("Conflicts with {} `{}` ({}): {} wins{}.", other.decision, other.text, other.from, other.decision, if both > 0 { format!(", on {both} call{} this window", if both == 1 { "" } else { "s" }) } else { String::new() }),
                    }));
                }
            }
            // Shadowed: another allow already covers every call it decides.
            if let Some(p) = prefix_of(&r.text) {
                if let Some(wider) = rules.iter().filter(|o| o.decision == "allow" && o.text != r.text).find(|o| prefix_of(&o.text).is_some_and(|q| p.starts_with(&q) && (p.len() == q.len() || p[q.len()..].starts_with(' ')))) {
                    out.push(json!({ "rule": "shadowed", "pattern": r.text, "severity": "info", "message": format!("Shadowed: `{}` ({}) already allows it.", wider.text, wider.from) }));
                }
            }
        }
    }
    // Widening, over the whole version.
    let w = widening(current, rules, calls, cwd);
    if !w.is_empty() {
        for r in rules.iter().filter(|r| r.decision == "allow" && !current.iter().any(|c| c.decision == "allow" && c.text == r.text)) {
            let mine: Vec<&Json> = w.iter().filter(|x| x["what"] == json!(format!("rule {}", r.text)) || calls.iter().any(|c| hits(&r.text, &c.probe) && x["what"].as_str().is_some_and(|s| c.probe.command.as_deref().is_some_and(|cmd| s.ends_with(cmd))))).collect();
            if !mine.is_empty() {
                out.push(json!({ "rule": "widens", "pattern": r.text, "severity": "warn", "message": format!("Widens what agents may do: {} that asked or was denied before would run without asking.", plural(mine.len(), "call or pattern", "calls or patterns")) }));
            }
        }
    }
    out
}

pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ---------------------------------------------------------------- impact

/// What `after` would have changed in the window against `before` (the
/// current rules), counted call by call: prompts saved and added, denies
/// changed, waiting saved, by agent. A prompt the current rules allow
/// already is `already`, not the change's.
pub fn impact(before: &[Rule], after: &[Rule], calls: &[Call]) -> Json {
    let mut prompts_before = 0;
    let mut prompts_after = 0;
    let mut saved = 0;
    let mut added = 0;
    let mut already = 0;
    let mut wait_saved = 0i64;
    let mut denies_changed = 0;
    let mut by: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut rows = Vec::new();
    for c in calls {
        let (a, b) = (decide(before, &c.probe), decide(after, &c.probe));
        let e = by.entry(c.name.clone()).or_default();
        if c.prompted && a == "allow" {
            // It asked then, under an older version; the current rules
            // allow it now.
            if b == "allow" {
                already += 1;
            } else {
                prompts_after += 1;
                added += 1;
                e.1 += 1;
            }
        } else if c.prompted {
            prompts_before += 1;
            e.0 += 1;
            if b == "allow" {
                saved += 1;
                wait_saved += c.wait_ms;
                if c.outcome == "refused" {
                    denies_changed += 1;
                }
                rows.push(json!({ "agent": c.name, "ts": crate::db::iso(c.ts), "call": c.probe.command.clone().or(c.probe.path.clone()), "change": if c.outcome == "refused" { "refused → would run" } else { "asked → allowed" } }));
            } else {
                prompts_after += 1;
                e.1 += 1;
            }
        } else if c.outcome == "ran" && a == "allow" && b != "allow" {
            prompts_after += 1;
            added += 1;
            e.1 += 1;
            rows.push(json!({ "agent": c.name, "ts": crate::db::iso(c.ts), "call": c.probe.command.clone().or(c.probe.path.clone()), "change": if b == "deny" { "ran → denied" } else { "ran → would ask" } }));
        }
        if a == "deny" && b != "deny" {
            denies_changed += 1;
        }
    }
    rows.truncate(200);
    json!({
        "level": "counted",
        "calls": calls.len(),
        "prompts_before": prompts_before,
        "prompts_after": prompts_after,
        "saved": saved,
        "added": added,
        "already": already,
        "denies_changed": denies_changed,
        "wait_saved_ms": wait_saved,
        "by_agent": by.into_iter().map(|(k, (a, b))| json!({ "agent": k, "before": a, "after": b })).collect::<Vec<_>>(),
        "rows": rows,
    })
}

/// Hits of each rule in the window, with the last one.
pub fn rule_rows(rules: &[Rule], calls: &[Call]) -> Vec<Json> {
    rules
        .iter()
        .map(|r| {
            let hit: Vec<&Call> = calls.iter().filter(|c| hits(&r.text, &c.probe)).collect();
            let decided = hit.iter().filter(|c| decide(rules, &c.probe) == r.decision || r.decision == "allow" && decide(rules, &c.probe) == "allow").count();
            json!({ "decision": r.decision, "text": r.text, "from": r.from, "hits": hit.len(), "decided": decided, "last": hit.iter().map(|c| c.ts).max().map(crate::db::iso) })
        })
        .collect()
}

// ------------------------------------------------------------ autocomplete

/// The prefixes worth a rule for a command as typed: its first simple
/// command (after `cd`) cut at 1 to 4 words, the `VAR=` words kept.
fn prefixes(cmd: &str) -> Vec<String> {
    let Some(first) = parts(cmd).into_iter().find(|p| !p.starts_with("cd ") && p != "cd") else { return Vec::new() };
    let words: Vec<&str> = first.split_whitespace().collect();
    let vars = words.iter().take_while(|w| w.contains('=') && !w.starts_with('-')).count();
    let plain = |w: &&str| !w.contains(['"', '\'', '$', '`', '(', ')', '*', '>', '<']);
    let mut out = Vec::new();
    for n in 1..=4usize {
        let k = vars + n;
        if k > words.len() || !words[..k].iter().all(plain) {
            break;
        }
        out.push(words[..k].join(" "));
    }
    out
}

/// Completions for a rule being typed (`q`, with or without `Bash(`):
/// commands agents ran here, by how often they were asked for and what the
/// user answered. Each `{insert, label, asked, allowed, denied, runs,
/// agents, sessions, note}`.
pub fn complete(calls: &[Call], current: &[Rule], q: &str) -> Vec<Json> {
    let q = q.trim().trim_start_matches("Bash(").trim_end_matches(')').trim_end_matches(":*").to_string();
    #[derive(Default)]
    struct Agg {
        runs: usize,
        asked: usize,
        allowed: usize,
        denied: usize,
        agents: HashSet<String>,
        last_denied: Option<(String, i64)>,
    }
    let mut by: HashMap<String, Agg> = HashMap::new();
    for c in calls.iter().filter(|c| c.probe.tool == "Bash") {
        let Some(cmd) = c.probe.command.as_deref() else { continue };
        for p in prefixes(cmd) {
            if !q.is_empty() && !p.starts_with(&q) && !q.starts_with(&p) {
                continue;
            }
            if p.len() < q.len() {
                continue;
            }
            let a = by.entry(p).or_default();
            a.runs += 1;
            a.agents.insert(c.name.clone());
            if c.prompted {
                a.asked += 1;
                if c.outcome == "refused" {
                    a.denied += 1;
                    if a.last_denied.as_ref().is_none_or(|(_, t)| c.ts > *t) {
                        a.last_denied = Some((c.name.clone(), c.ts));
                    }
                } else {
                    a.allowed += 1;
                }
            }
        }
    }
    let mut list: Vec<(String, Agg)> = by.into_iter().collect();
    list.sort_by(|a, b| b.1.asked.cmp(&a.1.asked).then(b.1.runs.cmp(&a.1.runs)).then(a.0.len().cmp(&b.0.len())));
    list.into_iter()
        .take(8)
        .map(|(p, a)| {
            let rule = format!("Bash({p}:*)");
            let would = calls.iter().filter(|c| c.prompted && hits(&rule, &c.probe) && decide(current, &c.probe) != "allow").count();
            let note = if let Some((who, t)) = &a.last_denied {
                format!("you denied it {} ({who}, {})", if a.denied == 1 { "once".to_string() } else { format!("{} times", a.denied) }, &crate::db::iso(*t)[..10])
            } else {
                let mut names: Vec<&String> = a.agents.iter().collect();
                names.sort();
                format!("{} · {}", names.iter().take(3).map(|s| s.as_str()).collect::<Vec<_>>().join(", "), plural(a.agents.len(), "agent", "agents"))
            };
            json!({ "insert": rule, "label": format!("{p}:*"), "asked": a.asked, "allowed": a.allowed, "denied": a.denied, "runs": a.runs, "agents": a.agents.len(), "saves": would, "note": note, "source": "ran here" })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(d: &str, t: &str) -> Rule {
        Rule { decision: d.into(), text: t.into(), from: "t".into() }
    }
    fn bash(c: &str) -> Probe {
        Probe { tool: "Bash".into(), command: Some(c.into()), path: None, cwd: "/w".into() }
    }

    #[test]
    fn prefix_rules_match_as_typed() {
        let rules = vec![r("allow", "Bash(python3 -m pytest:*)"), r("deny", "Bash(git push:*)")];
        assert_eq!(decide(&rules, &bash("python3 -m pytest -q")), "allow");
        assert_eq!(decide(&rules, &bash("PYTHONPATH=. python3 -m pytest -q")), "prompt");
        assert_eq!(decide(&rules, &bash("python3 -m pytest && git push")), "deny");
        assert_eq!(decide(&rules, &bash("python3 -m pytestx")), "prompt");
        assert_eq!(decide(&rules, &bash("cd tests && python3 -m pytest")), "allow");
        let e = Probe { tool: "Write".into(), command: None, path: Some("/w/a/b.py".into()), cwd: "/w".into() };
        assert_eq!(decide(&[r("allow", "Edit(/<worktree>/**)")], &e), "allow");
        let out = Probe { path: Some("/x/b.py".into()), ..e };
        assert_eq!(decide(&[r("allow", "Edit(/<worktree>/**)")], &out), "prompt");
        assert_eq!(decide(&[r("allow", "mcp__workbench")], &Probe { tool: "mcp__workbench__ask_human".into(), ..Probe::default() }), "allow");
    }

    #[test]
    fn impact_counts_against_the_current_rules() {
        let call = |c: &str| Call { agent: "a".into(), name: "a".into(), ts: 0, probe: bash(c), prompted: true, outcome: "ran", wait_ms: 1000, mode: "default".into() };
        let calls = vec![call("PYTHONPATH=. python3 -m pytest -q"), call("npm publish")];
        // The storm's rule is current already: a draft adding npm's saves one.
        let cur = vec![r("allow", "Bash(PYTHONPATH=. python3 -m pytest:*)")];
        let draft = vec![r("allow", "Bash(PYTHONPATH=. python3 -m pytest:*)"), r("allow", "Bash(npm publish:*)")];
        let i = impact(&cur, &draft, &calls);
        assert_eq!((i["saved"].as_i64(), i["already"].as_i64(), i["prompts_before"].as_i64(), i["prompts_after"].as_i64()), (Some(1), Some(1), Some(1), Some(0)), "{i}");
        // Dropping the current rule makes the storm ask again.
        let i = impact(&cur, &[], &calls);
        assert_eq!((i["saved"].as_i64(), i["added"].as_i64(), i["prompts_after"].as_i64()), (Some(0), Some(1), Some(2)), "{i}");
    }

    #[test]
    fn widening_is_found_over_the_whole_version() {
        let before = vec![r("allow", "Bash(git status:*)")];
        let after = vec![r("allow", "Bash(git status:*)"), r("allow", "Bash(python3:*)")];
        let w = widening(&before, &after, &[], "/w");
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(widening(&after, &before, &[], "/w").is_empty());
        // Narrowing is not widening.
        let narrow = vec![r("allow", "Bash(git status:*)"), r("deny", "Bash(rm:*)")];
        assert!(widening(&before, &narrow, &[], "/w").is_empty());
    }

    #[test]
    fn secrets_are_found() {
        assert_eq!(secrets(r#"{"env": {"STRIPE_TEST_KEY": "sk_test_abcdefghijkl"}}"#).len(), 1);
        assert!(secrets(r#"{"allow": ["Bash(git status:*)"]}"#).is_empty());
        assert_eq!(secrets("API_TOKEN=abcdefgh1234").len(), 1);
    }

    #[test]
    fn edits_round_trip_through_the_block() {
        let base = vec![r("allow", "Read(/<worktree>/**)"), r("allow", "Bash(git status:*)")];
        let e = PermEdits { allow: vec!["Bash(make:*)".into()], deny: vec!["Bash(rm:*)".into()], remove: vec!["Bash(git status:*)".into()], ..PermEdits::default() };
        let rules = effective(&base, &e, "you");
        let back = edits_of_block(&base, &block(&rules)).unwrap();
        assert_eq!(back, e);
        assert!(edits_of_block(&base, &json!({"allow": "x"})).is_err());
    }
}
