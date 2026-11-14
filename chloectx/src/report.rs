//! `chloectx report`: where a project's time went, from the records the
//! store already has. It reads and writes nothing else.
//!
//! Each session's records become points in time: a prompt, a tool call's
//! start and its result, the answer, a notification, a compaction's start
//! and its summary. The time between two points goes to one of:
//!
//! - **model**: from a prompt or a tool's result to the next tool call or
//!   the answer;
//! - **tools**: from a call to its result (while any call runs);
//! - **waiting**: from an answer, or a notification (a permission prompt,
//!   an idle notice), to the next prompt, and anything that ends in a
//!   prompt (a turn the user interrupted). One wait counts up to
//!   [`WAIT_CAP`]; the rest of it is **away**, not counted with the others.
//!   A permission wait lasts until the tool's result, so it holds the
//!   tool's own run after the user answered;
//! - **compaction**: from a compaction's start (`PreCompact`) to its
//!   summary; for a compaction recorded before ChloeCtx kept its start, the
//!   gap before the summary unless the user was being waited for.
//!
//! Records keep Unix seconds, so every span is in whole seconds. A tool call
//! knows when it started only from the `Took:` line its record carries
//! ([`crate::capture::took`]): calls recorded before ChloeCtx timed them,
//! and calls under a second, count as instants at their result.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use crate::capture::{self, NOTIFICATION, SUBAGENT_STOP};
use crate::fixes;
use crate::record::{Kind, Record};
use crate::recall::fmt_time;

/// The longest one wait for the user counts; the rest of it is away.
pub const WAIT_CAP: i64 = 30 * 60;
/// An error seen this many times is listed.
pub const ERROR_MIN: usize = 3;
/// A file read this many times in one session is listed.
pub const REREAD_MIN: usize = 4;
/// The tag of the event a compaction's start leaves.
pub const COMPACT_START: &str = "compact_start";

#[derive(Clone, Debug)]
pub struct Options {
    /// What the report names as the project.
    pub project: String,
    pub days: f64,
    /// Only the sessions whose id starts with this.
    pub session: Option<String>,
    pub now: i64,
}

/// Seconds by what they went to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Split {
    pub model: i64,
    pub tools: i64,
    pub waiting: i64,
    /// Of `waiting`, the waits on a permission prompt.
    pub permission: i64,
    pub compaction: i64,
    /// Waits past [`WAIT_CAP`], counted in none of the others.
    pub away: i64,
}

impl Split {
    fn add(&mut self, o: &Split) {
        self.model += o.model;
        self.tools += o.tools;
        self.waiting += o.waiting;
        self.permission += o.permission;
        self.compaction += o.compaction;
        self.away += o.away;
    }

    /// Model, tools, waiting and compaction: the time a session was in use.
    pub fn active(&self) -> i64 {
        self.model + self.tools + self.waiting + self.compaction
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionReport {
    pub session: String,
    /// The folder its hooks ran in (a worktree), when the store knows it.
    pub worktree: Option<String>,
    pub start: i64,
    pub end: i64,
    pub turns: usize,
    pub tool_calls: usize,
    pub split: Split,
    pub compactions: usize,
    pub permission_prompts: usize,
    pub subagents: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CommandTime {
    /// The command as error to fix matches it ([`fixes::command_key`]).
    pub command: String,
    pub runs: usize,
    /// Runs whose record says how long they took.
    pub timed: usize,
    pub total_secs: i64,
    pub max_secs: i64,
    pub failed: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolTime {
    pub tool: String,
    pub calls: usize,
    pub total_secs: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepeatedError {
    /// The error's line with what changes from run to run masked ([`fixes::signature`]).
    pub signature: String,
    /// One of its lines as the output had it.
    pub line: String,
    pub count: usize,
    pub sessions: usize,
    /// The command (or tool) it came from most often.
    pub from: String,
    /// The session it came back most often in, how often, and over how long.
    pub top_session: String,
    pub top_count: usize,
    pub top_span_secs: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Reread {
    pub file: String,
    pub session: String,
    pub reads: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Suggestion {
    /// `slow_command`, `error_loop`, `reread`, `permission`, `compaction`.
    pub kind: String,
    pub text: String,
    /// What it is ranked by: about how many seconds it bears on.
    pub seconds: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub project: String,
    pub days: f64,
    pub since: i64,
    pub until: i64,
    /// Every time is in whole seconds.
    pub resolution: &'static str,
    pub wait_cap_secs: i64,
    pub total: Split,
    pub turns: usize,
    pub tool_calls: usize,
    /// Calls whose record says how long they took.
    pub timed_calls: usize,
    pub compactions: usize,
    /// Compactions per hour of active time.
    pub compactions_per_hour: f64,
    pub permission_prompts: usize,
    pub subagents: usize,
    pub sessions: Vec<SessionReport>,
    pub commands: Vec<CommandTime>,
    pub tools: Vec<ToolTime>,
    pub errors: Vec<RepeatedError>,
    pub rereads: Vec<Reread>,
    pub suggestions: Vec<Suggestion>,
}

// ---------------------------------------------------------------- one session

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum P {
    Prompt,
    CallStart,
    CallEnd,
    Answer,
    /// A notification that the agent waits for the user; `true` for a permission prompt.
    Wait(bool),
    CompactStart,
    CompactEnd,
}

/// A tool call: the call's record and what came of it.
#[derive(Clone, Debug)]
struct Call {
    tool: String,
    text: String,
    start: i64,
    end: i64,
    files: String,
    error: Option<String>,
}

/// Seconds of one session's points, by what they went to.
fn split(points: &mut [(i64, i64, P)]) -> Split {
    points.sort_by_key(|p| (p.0, p.1));
    let mut out = Split::default();
    let (mut running, mut turn_open, mut compacting) = (0usize, false, false);
    // The wait under way: whether on a permission, and how long so far.
    let mut wait: Option<bool> = None;
    let mut waited = 0i64;
    for i in 0..points.len() {
        let (t, _, p) = points[i];
        if i > 0 {
            let gap = (t - points[i - 1].0).max(0);
            let waiting_now = if p == P::Prompt {
                Some(wait.unwrap_or(false))
            } else if compacting || (p == P::CompactEnd && wait.is_none() && turn_open) {
                None
            } else if wait.is_some() {
                wait
            } else if running > 0 || turn_open {
                None
            } else {
                Some(false)
            };
            match waiting_now {
                Some(perm) => {
                    let counted = gap.min((WAIT_CAP - waited).max(0));
                    waited += gap;
                    out.waiting += counted;
                    if perm {
                        out.permission += counted;
                    }
                    out.away += gap - counted;
                }
                None => {
                    waited = 0;
                    if compacting || p == P::CompactEnd {
                        out.compaction += gap;
                    } else if running > 0 {
                        out.tools += gap;
                    } else {
                        out.model += gap;
                    }
                }
            }
        }
        match p {
            P::Prompt => {
                turn_open = true;
                wait = None;
                waited = 0;
            }
            P::CallStart => {
                running += 1;
                wait = None;
                waited = 0;
            }
            P::CallEnd => {
                running = running.saturating_sub(1);
                wait = None;
                waited = 0;
            }
            P::Answer => {
                turn_open = false;
                wait = Some(false);
            }
            P::Wait(perm) => {
                if wait.is_none() {
                    waited = 0;
                }
                wait = Some(perm || wait == Some(true));
            }
            P::CompactStart => compacting = true,
            P::CompactEnd => compacting = false,
        }
    }
    out
}

/// What one session's records say, in sequence order.
struct Parsed {
    points: Vec<(i64, i64, P)>,
    calls: Vec<(i64, Call)>,
    turns: usize,
    compactions: usize,
    permission: Vec<(i64, String)>,
    subagents: usize,
}

fn parse(recs: &[&Record]) -> Parsed {
    let mut out = Parsed { points: Vec::new(), calls: Vec::new(), turns: 0, compactions: 0, permission: Vec::new(), subagents: 0 };
    let mut last_prompt_turn = None;
    let mut last_answer_turn = None;
    let mut last_summary_ts = None;
    let mut i = 0;
    while i < recs.len() {
        let r = recs[i];
        let ord = r.seq * 2;
        match r.kind {
            Kind::Prompt if last_prompt_turn != Some(r.turn) => {
                last_prompt_turn = Some(r.turn);
                out.turns += 1;
                out.points.push((r.ts, ord, P::Prompt));
            }
            Kind::Answer | Kind::Episode if r.tag != "narration" && last_answer_turn != Some(r.turn) && last_prompt_turn == Some(r.turn) => {
                last_answer_turn = Some(r.turn);
                out.points.push((r.ts, ord, P::Answer));
            }
            Kind::Summary if last_summary_ts != Some(r.ts) => {
                last_summary_ts = Some(r.ts);
                out.compactions += 1;
                out.points.push((r.ts, ord, P::CompactEnd));
            }
            Kind::Event if r.tag == COMPACT_START => out.points.push((r.ts, ord, P::CompactStart)),
            Kind::Event if r.tag == SUBAGENT_STOP => out.subagents += 1,
            Kind::Event if r.tag.starts_with(NOTIFICATION) => {
                let ty = r.tag.split_once(':').map_or("", |x| x.1);
                match ty {
                    "permission_prompt" => {
                        let tool = r.text.split_once("permission to use ").map(|x| x.1.trim().trim_end_matches('.').to_string()).unwrap_or_default();
                        out.permission.push((r.ts, tool));
                        out.points.push((r.ts, ord, P::Wait(true)));
                    }
                    "idle_prompt" | "elicitation_dialog" => out.points.push((r.ts, ord, P::Wait(false))),
                    _ => {}
                }
            }
            Kind::ToolCall => {
                let took = capture::took_of(&r.ctx);
                let mut c = Call { tool: r.tag.clone(), text: r.text.clone(), start: r.ts - took, end: r.ts, files: r.files.clone(), error: None };
                // Its results: the records stored with it.
                let mut j = i + 1;
                while j < recs.len() && matches!(recs[j].kind, Kind::ToolResult | Kind::Error) && recs[j].tag == r.tag && recs[j].ts == r.ts {
                    if recs[j].kind == Kind::Error && c.error.is_none() {
                        c.error = Some(recs[j].text.clone());
                    }
                    j += 1;
                }
                // The call's record is stored at its result, after what
                // happened while it ran (a permission prompt): its start
                // goes right after the last record of the conversation
                // made by then, before the events of that second.
                let before = recs[..i].iter().rev().find(|x| x.ts <= c.start && x.kind != Kind::Event).map_or(0, |x| x.seq * 2 + 1);
                out.points.push((c.start, before, P::CallStart));
                out.points.push((c.end, ord + 1, P::CallEnd));
                out.calls.push((r.seq, c));
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// The command of a Bash call's record (`$ cmd  # description`).
fn command_of(text: &str) -> &str {
    let t = text.strip_prefix("$ ").unwrap_or(text);
    t.rsplit_once("  # ").map_or(t, |x| x.0)
}

// ---------------------------------------------------------------- the report

/// The report over `records` (any order), for the sessions `opts` names;
/// `worktrees` holds the folder each session's hooks ran in.
pub fn build(records: &[Record], worktrees: &HashMap<String, String>, opts: &Options) -> Report {
    let since = opts.now - (opts.days * 86_400.0) as i64;
    let mut by_session: BTreeMap<&str, Vec<&Record>> = BTreeMap::new();
    for r in records {
        if r.kind == Kind::Entity || r.ts < since || r.session == "-" {
            continue;
        }
        if opts.session.as_deref().is_some_and(|s| !r.session.starts_with(s)) {
            continue;
        }
        by_session.entry(r.session.as_str()).or_default().push(r);
    }
    let mut sessions = Vec::new();
    let mut total = Split::default();
    let (mut turns, mut compactions, mut subagents, mut tool_calls, mut timed_calls) = (0, 0, 0, 0, 0);
    let mut permission: Vec<(i64, String)> = Vec::new();
    let mut commands: HashMap<String, CommandTime> = HashMap::new();
    let mut tools: HashMap<String, ToolTime> = HashMap::new();
    // signature -> (line, [(session, ts, from)])
    let mut errors: HashMap<String, (String, Vec<(String, i64, String)>)> = HashMap::new();
    let mut rereads = Vec::new();
    for (sid, mut recs) in by_session {
        recs.sort_by_key(|r| r.seq);
        let mut parsed = parse(&recs);
        let s = split(&mut parsed.points);
        let (start, end) = (parsed.points.first().map_or(0, |p| p.0), parsed.points.last().map_or(0, |p| p.0));
        if parsed.points.is_empty() {
            continue;
        }
        let worktree = worktrees.get(sid).cloned().filter(|w| !w.is_empty());
        let mask_root = worktree.clone().unwrap_or_else(|| opts.project.clone());
        let mut reads: HashMap<String, usize> = HashMap::new();
        for (_, c) in &parsed.calls {
            let secs = c.end - c.start;
            if secs > 0 {
                timed_calls += 1;
            }
            let t = tools.entry(c.tool.clone()).or_insert_with(|| ToolTime { tool: c.tool.clone(), calls: 0, total_secs: 0 });
            t.calls += 1;
            t.total_secs += secs;
            let from = if c.tool == "Bash" { fixes::command_key(command_of(&c.text)) } else { c.tool.clone() };
            if c.tool == "Bash" && !from.is_empty() {
                let e = commands.entry(from.clone()).or_insert_with(|| CommandTime { command: from.clone(), runs: 0, timed: 0, total_secs: 0, max_secs: 0, failed: 0 });
                e.runs += 1;
                e.timed += usize::from(secs > 0);
                e.total_secs += secs;
                e.max_secs = e.max_secs.max(secs);
                e.failed += usize::from(c.error.is_some());
            }
            if let Some(text) = &c.error {
                if let Some((sig, line)) = fixes::signature(text, &mask_root) {
                    let e = errors.entry(sig).or_insert_with(|| (line, Vec::new()));
                    e.1.push((sid.to_string(), c.end, from.clone()));
                }
            }
            if c.tool == "Read" && !c.files.is_empty() {
                *reads.entry(c.files.split(' ').next().unwrap_or("").to_string()).or_default() += 1;
            }
        }
        for (file, n) in reads {
            if n >= REREAD_MIN && !file.is_empty() {
                rereads.push(Reread { file, session: sid.to_string(), reads: n });
            }
        }
        total.add(&s);
        turns += parsed.turns;
        compactions += parsed.compactions;
        subagents += parsed.subagents;
        tool_calls += parsed.calls.len();
        permission.extend(parsed.permission.iter().cloned());
        sessions.push(SessionReport {
            session: sid.to_string(),
            worktree,
            start,
            end,
            turns: parsed.turns,
            tool_calls: parsed.calls.len(),
            split: s,
            compactions: parsed.compactions,
            permission_prompts: parsed.permission.len(),
            subagents: parsed.subagents,
        });
    }
    sessions.sort_by_key(|s| s.start);

    let mut commands: Vec<CommandTime> = commands.into_values().collect();
    commands.sort_by(|a, b| b.total_secs.cmp(&a.total_secs).then(b.runs.cmp(&a.runs)).then(a.command.cmp(&b.command)));
    commands.truncate(8);
    let mut tools: Vec<ToolTime> = tools.into_values().collect();
    tools.sort_by(|a, b| b.total_secs.cmp(&a.total_secs).then(b.calls.cmp(&a.calls)).then(a.tool.cmp(&b.tool)));
    let mut errors: Vec<RepeatedError> = errors
        .into_iter()
        .filter(|(_, (_, seen))| seen.len() >= ERROR_MIN)
        .map(|(signature, (line, seen))| {
            let sessions: HashSet<&str> = seen.iter().map(|s| s.0.as_str()).collect();
            let mut per: HashMap<&str, (usize, i64, i64)> = HashMap::new();
            for (s, ts, _) in &seen {
                let e = per.entry(s.as_str()).or_insert((0, *ts, *ts));
                e.0 += 1;
                e.1 = e.1.min(*ts);
                e.2 = e.2.max(*ts);
            }
            let (top, (n, first, last)) = per.iter().max_by_key(|(s, v)| (v.0, std::cmp::Reverse(**s))).map(|(s, v)| (s.to_string(), *v)).unwrap_or_default();
            let mut froms: HashMap<&str, usize> = HashMap::new();
            for (_, _, f) in &seen {
                *froms.entry(f.as_str()).or_default() += 1;
            }
            let from = froms.into_iter().max_by_key(|(f, n)| (*n, std::cmp::Reverse(*f))).map(|x| x.0.to_string()).unwrap_or_default();
            RepeatedError { signature, line, count: seen.len(), sessions: sessions.len(), from, top_session: top, top_count: n, top_span_secs: last - first }
        })
        .collect();
    errors.sort_by(|a, b| b.count.cmp(&a.count).then(a.signature.cmp(&b.signature)));
    errors.truncate(8);
    rereads.sort_by(|a, b| b.reads.cmp(&a.reads).then(a.file.cmp(&b.file)));
    rereads.truncate(8);

    let hours = total.active() as f64 / 3600.0;
    let compactions_per_hour = if hours > 0.0 { (compactions as f64 / hours * 10.0).round() / 10.0 } else { 0.0 };
    let mut report = Report {
        project: opts.project.clone(),
        days: opts.days,
        since,
        until: opts.now,
        resolution: "seconds",
        wait_cap_secs: WAIT_CAP,
        total,
        turns,
        tool_calls,
        timed_calls,
        compactions,
        compactions_per_hour,
        permission_prompts: permission.len(),
        subagents,
        sessions,
        commands,
        tools,
        errors,
        rereads,
        suggestions: Vec::new(),
    };
    report.suggestions = suggest(&report, &permission);
    report
}

/// Up to five suggestions, the ones bearing on the most time first, each
/// with the numbers behind it.
fn suggest(r: &Report, permission: &[(i64, String)]) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let short = |s: &str| s.chars().take(8).collect::<String>();
    for c in &r.commands {
        let avg = if c.timed > 0 { c.total_secs / c.timed as i64 } else { 0 };
        if c.runs >= 3 && c.total_secs >= 120 && avg >= 15 {
            let failed = if c.failed > 0 { format!(", and failed {} of those times", c.failed) } else { String::new() };
            out.push(Suggestion {
                kind: "slow_command".into(),
                text: format!(
                    "`{}` ran {} times and took {} in all ({} a run{failed}). A narrower run while iterating (one test, one package, an incremental or cached build) would save most of that.",
                    clip(&c.command, 80), c.runs, dur(c.total_secs), dur(avg)
                ),
                seconds: c.total_secs,
            });
        }
    }
    for e in &r.errors {
        if e.top_count >= ERROR_MIN {
            out.push(Suggestion {
                kind: "error_loop".into(),
                text: format!(
                    "The same error came back {} times in session {} over {} (`{}`, from `{}`): what was tried did not reach its cause. Read the whole error, or look up how it was fixed before (`chloectx search`), before trying again.",
                    e.top_count, short(&e.top_session), dur(e.top_span_secs), clip(&e.line, 100), clip(&e.from, 60)
                ),
                seconds: e.top_span_secs.max(60 * e.top_count as i64),
            });
        }
    }
    for x in r.rereads.iter().take(2) {
        out.push(Suggestion {
            kind: "reread".into(),
            text: format!(
                "`{}` was read {} times in session {}: a short note on its layout in CLAUDE.md, or reading only the lines needed, would spare the rereads and the context they fill.",
                x.file, x.reads, short(&x.session)
            ),
            seconds: 20 * x.reads as i64,
        });
    }
    if permission.len() >= 3 {
        let mut by_tool: HashMap<&str, usize> = HashMap::new();
        for (_, t) in permission {
            *by_tool.entry(if t.is_empty() { "a tool" } else { t.as_str() }).or_default() += 1;
        }
        let mut tools: Vec<(&str, usize)> = by_tool.into_iter().collect();
        tools.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let named: Vec<String> = tools.iter().take(3).map(|(t, n)| format!("{t} {n} times")).collect();
        out.push(Suggestion {
            kind: "permission".into(),
            text: format!(
                "Claude asked for permission {} times ({}) and waited {} in all for the answers: allowing the ones you always approve in the project's `.claude/settings.json` (`permissions.allow`) would spare the waits.",
                permission.len(), named.join(", "), dur(r.total.permission)
            ),
            seconds: r.total.permission.max(30 * permission.len() as i64),
        });
    }
    if r.compactions >= 2 && r.compactions_per_hour >= 1.0 {
        out.push(Suggestion {
            kind: "compaction".into(),
            text: format!(
                "The context was compacted {} times, {} an hour of work ({} compacting), and each loses detail: a new session per task, and long outputs (logs, whole files) kept out of the conversation, make it rarer.",
                r.compactions, r.compactions_per_hour, dur(r.total.compaction)
            ),
            seconds: r.total.compaction + 300 * r.compactions as i64,
        });
    }
    out.sort_by(|a, b| b.seconds.cmp(&a.seconds).then(a.kind.cmp(&b.kind)));
    out.truncate(5);
    out
}

fn clip(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        format!("{}…", one.chars().take(max - 1).collect::<String>())
    }
}

/// `42s`, `3m 05s`, `30m`, `2h 07m`.
pub fn dur(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 && s % 60 == 0 {
        format!("{}m", s / 60)
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn pct(part: i64, whole: i64) -> String {
    if whole <= 0 {
        "-".into()
    } else {
        format!("{}%", (part as f64 * 100.0 / whole as f64).round() as i64)
    }
}

/// The report as text.
pub fn render(r: &Report) -> String {
    let mut o = String::new();
    let days = if r.days.fract() == 0.0 { format!("{}", r.days as i64) } else { format!("{}", r.days) };
    o += &format!("Where the time went in {}, the last {days} day{} ({} to {})\n", r.project, if r.days == 1.0 { "" } else { "s" }, fmt_time(r.since), fmt_time(r.until));
    o += "Times are whole seconds: the store keeps each record's time in Unix seconds.\n\n";
    if r.sessions.is_empty() {
        o += "No sessions recorded in that time.\n";
        return o;
    }
    let t = &r.total;
    let active = t.active();
    o += &format!(
        "{} session{}, {} turns, {} tool calls; {} in use\n",
        r.sessions.len(),
        if r.sessions.len() == 1 { "" } else { "s" },
        r.turns,
        r.tool_calls,
        dur(active)
    );
    let perm = if r.permission_prompts > 0 { format!(" ({} permission prompt{}, {})", r.permission_prompts, if r.permission_prompts == 1 { "" } else { "s" }, dur(t.permission)) } else { String::new() };
    let rows = [
        ("model", t.model, "from a prompt or a tool's result to the next tool call or the answer".to_string()),
        ("tools", t.tools, format!("from a call to its result ({} of {} calls timed; the others took under a second or were recorded before calls were timed)", r.timed_calls, r.tool_calls)),
        ("waiting", t.waiting, format!("for the user: from an answer or a notification to the next prompt{perm}")),
        ("compaction", t.compaction, format!("{} compaction{}, {} an hour", r.compactions, if r.compactions == 1 { "" } else { "s" }, r.compactions_per_hour)),
    ];
    for (name, secs, what) in rows {
        o += &format!("  {name:<11}{:>8}  {:>4}  {what}\n", dur(secs), pct(secs, active));
    }
    o += &format!("  {:<11}{:>8}        waits past {}, not counted above\n", "away", dur(t.away), dur(r.wait_cap_secs));
    if r.subagents > 0 {
        o += &format!("  {} built-in sub-agent{} finished\n", r.subagents, if r.subagents == 1 { "" } else { "s" });
    }

    o += "\nSessions\n";
    o += &format!("  {:<9} {:<17} {:<16} {:>5} {:>8} {:>8} {:>8} {:>8} {:>8}\n", "session", "started", "worktree", "turns", "model", "tools", "waiting", "compact", "away");
    for s in &r.sessions {
        let wt = s.worktree.as_deref().map(|w| std::path::Path::new(w).file_name().map_or(w.to_string(), |n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "-".into());
        let started = fmt_time(s.start);
        o += &format!(
            "  {:<9} {:<17} {:<16} {:>5} {:>8} {:>8} {:>8} {:>8} {:>8}\n",
            s.session.chars().take(8).collect::<String>(),
            started.trim_end_matches(" UTC"),
            clip(&wt, 16),
            s.turns,
            dur(s.split.model),
            dur(s.split.tools),
            dur(s.split.waiting),
            dur(s.split.compaction),
            dur(s.split.away)
        );
    }

    if !r.commands.is_empty() {
        o += "\nCommands that took most time (normalised as error to fix matches them)\n";
        for c in &r.commands {
            let each = if c.timed > 0 { format!(", {} a run", dur(c.total_secs / c.timed as i64)) } else { String::new() };
            let failed = if c.failed > 0 { format!(", {} failed", c.failed) } else { String::new() };
            o += &format!("  {:>8}  {} run{}{each}{failed}   {}\n", dur(c.total_secs), c.runs, if c.runs == 1 { "" } else { "s" }, clip(&c.command, 90));
        }
    }
    if !r.tools.is_empty() {
        let parts: Vec<String> = r.tools.iter().take(8).map(|t| format!("{} {} ({} call{})", t.tool, dur(t.total_secs), t.calls, if t.calls == 1 { "" } else { "s" })).collect();
        o += &format!("\nTools by time: {}\n", parts.join(", "));
    }
    if !r.errors.is_empty() {
        o += &format!("\nErrors seen {ERROR_MIN} or more times\n");
        for e in &r.errors {
            o += &format!("  {}x in {} session{}  {}   (from {})\n", e.count, e.sessions, if e.sessions == 1 { "" } else { "s" }, clip(&e.line, 100), clip(&e.from, 50));
        }
    }
    if !r.rereads.is_empty() {
        o += &format!("\nFiles read {REREAD_MIN} or more times in one session\n");
        for x in &r.rereads {
            o += &format!("  {}x  {}   (session {})\n", x.reads, x.file, x.session.chars().take(8).collect::<String>());
        }
    }
    o += "\nSuggestions\n";
    if r.suggestions.is_empty() {
        o += "  None: no slow command run often, error loop, file reread often, frequent permission prompt or compaction stands out.\n";
    }
    for (i, s) in r.suggestions.iter().enumerate() {
        o += &format!("  {}. {}\n", i + 1, s.text);
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Draft, Record};

    fn rec(seq: i64, session: &str, turn: i64, ts: i64, d: Draft) -> Record {
        Record::from_draft(d, session, turn, seq, ts)
    }

    /// One session: two turns with tool calls (one timed, one an instant),
    /// a permission prompt, an idle wait past the cap and a compaction.
    fn session() -> Vec<Record> {
        let mut v = Vec::new();
        let mut seq = 0;
        let mut add = |turn: i64, ts: i64, d: Draft| {
            seq += 1;
            v.push(rec(seq, "s1abcdef99", turn, ts, d));
        };
        let t0 = 1_000_000;
        add(1, t0, Draft::new(Kind::Prompt, "Build it"));
        // Model 10 s, then a build of 50 s that fails.
        add(1, t0 + 60, Draft::new(Kind::ToolCall, "$ cargo build  # Build").tag("Bash").ctx("Turn 1 request: Build it\nTook: 50s"));
        add(1, t0 + 60, Draft::new(Kind::Error, "error[E0308]: mismatched types at src/a.rs:10").tag("Bash"));
        // Model 5 s, a read (instant).
        add(1, t0 + 65, Draft::new(Kind::ToolCall, "Read src/a.rs (10 lines)").tag("Read").files(vec!["src/a.rs".into()]));
        // Model 5 s; a call at 70 s that needs permission at 71 s and ends at 100 s.
        add(1, t0 + 71, Draft::new(Kind::Event, "Notification: Claude needs your permission to use Bash").tag("notification:permission_prompt"));
        add(1, t0 + 100, Draft::new(Kind::ToolCall, "$ rm -rf target").tag("Bash").ctx("Took: 30s"));
        // Model 20 s, the answer.
        add(1, t0 + 120, Draft::new(Kind::Answer, "Done."));
        add(1, t0 + 120, Draft::new(Kind::Episode, "Turn 1. User asked: Build it"));
        // An idle notice, then the next prompt an hour after the answer.
        add(1, t0 + 180, Draft::new(Kind::Event, "Notification: Claude is waiting for your input").tag("notification:idle_prompt"));
        add(2, t0 + 3720, Draft::new(Kind::Prompt, "Go on"));
        // Model 30 s, a compaction of 40 s, model 10 s, the answer.
        add(2, t0 + 3750, Draft::new(Kind::Event, "Compaction started (auto)").tag(COMPACT_START));
        add(2, t0 + 3790, Draft::new(Kind::Summary, "1. Primary Request").tag("primary_request_and_intent"));
        add(2, t0 + 3790, Draft::new(Kind::Summary, "2. Key Concepts").tag("key_technical_concepts"));
        add(2, t0 + 3800, Draft::new(Kind::Event, "Sub-agent Explore finished").tag(SUBAGENT_STOP));
        add(2, t0 + 3800, Draft::new(Kind::Answer, "All good."));
        v
    }

    fn opts() -> Options {
        Options { project: "/p".into(), days: 7.0, session: None, now: 1_000_000 + 86_400 }
    }

    #[test]
    fn time_is_split_by_what_it_went_to() {
        let r = build(&session(), &HashMap::new(), &opts());
        assert_eq!(r.sessions.len(), 1);
        let s = &r.total;
        // Model: 10 + 5 + 5 (to the call at 70) + 20 + 30 + 10.
        assert_eq!(s.model, 80, "{s:?}");
        // Tools: the build's 50 s, and the permission call's 70..71 before its prompt.
        assert_eq!(s.tools, 51, "{s:?}");
        // Waiting: 71..100 on the permission, then 120..3720 capped at 30 min.
        assert_eq!(s.permission, 29, "{s:?}");
        assert_eq!(s.waiting, 29 + WAIT_CAP, "{s:?}");
        assert_eq!(s.away, 3600 - WAIT_CAP, "{s:?}");
        assert_eq!(s.compaction, 40, "{s:?}");
        assert_eq!((r.turns, r.compactions, r.permission_prompts, r.subagents, r.tool_calls, r.timed_calls), (2, 1, 1, 1, 3, 2));
        // The whole session is accounted for.
        assert_eq!(s.active() + s.away, 3800);
        let text = render(&r);
        assert!(text.contains("Times are whole seconds"), "{text}");
        assert!(text.contains("cargo build"), "{text}");
    }

    /// A permission prompt in the second its call was made: the call's
    /// record comes after it, its start before it.
    #[test]
    fn a_permission_wait_runs_from_its_prompt_to_the_result() {
        let t0 = 3_000_000;
        let v = vec![
            rec(1, "s3", 1, t0, Draft::new(Kind::Prompt, "Clean up")),
            rec(2, "s3", 1, t0 + 4, Draft::new(Kind::Event, "Notification: Claude needs your permission to use Bash").tag("notification:permission_prompt")),
            rec(3, "s3", 1, t0 + 10, Draft::new(Kind::ToolCall, "$ rm -rf build").tag("Bash").ctx("Took: 6s")),
            rec(4, "s3", 1, t0 + 12, Draft::new(Kind::Answer, "Cleaned.")),
        ];
        let r = build(&v, &HashMap::new(), &Options { now: t0 + 100, ..opts() });
        assert_eq!((r.total.model, r.total.tools, r.total.waiting, r.total.permission), (6, 0, 6, 6), "{:?}", r.total);
    }

    #[test]
    fn repeats_rereads_and_suggestions() {
        let mut v = Vec::new();
        let mut seq = 0;
        let t0 = 2_000_000;
        v.push(rec(0, "s2", 1, t0, Draft::new(Kind::Prompt, "Fix the tests")));
        for i in 0..5 {
            let ts = t0 + 100 * (i + 1);
            seq += 3;
            // The same command, spelled differently, failing with the same error at other line numbers.
            let cmd = if i % 2 == 0 { "$ cd /w && cargo test --release 2>&1 | tail -20" } else { "$ cargo   test --release" };
            v.push(rec(seq, "s2", 1, ts, Draft::new(Kind::ToolCall, cmd).tag("Bash").ctx(format!("Took: {}s", 60 + i))));
            v.push(rec(seq + 1, "s2", 1, ts, Draft::new(Kind::Error, format!("error[E0599]: no method named `foo` found at /w/src/x.rs:{}", 10 + i)).tag("Bash")));
            v.push(rec(seq + 2, "s2", 1, ts + 5, Draft::new(Kind::ToolCall, "Read src/x.rs (40 lines)").tag("Read").files(vec!["src/x.rs".into()])));
        }
        for i in 0..3 {
            seq += 1;
            v.push(rec(seq, "s2", 1, t0 + 600 + i, Draft::new(Kind::Event, "Notification: Claude needs your permission to use Bash").tag("notification:permission_prompt")));
        }
        let wt = HashMap::from([("s2".to_string(), "/w".to_string())]);
        let r = build(&v, &wt, &Options { project: "/p".into(), days: 7.0, session: Some("s2".into()), now: t0 + 1000 });
        assert_eq!(r.commands.len(), 1, "{:?}", r.commands);
        let c = &r.commands[0];
        assert_eq!((c.command.as_str(), c.runs, c.failed, c.total_secs), ("cargo test --release", 5, 5, 310));
        assert_eq!(r.errors.len(), 1);
        assert_eq!((r.errors[0].count, r.errors[0].top_count, r.errors[0].from.as_str()), (5, 5, "cargo test --release"));
        assert!(r.errors[0].signature.contains("src/x.rs:#"), "{}", r.errors[0].signature);
        assert_eq!((r.rereads[0].file.as_str(), r.rereads[0].reads), ("src/x.rs", 5));
        let kinds: Vec<&str> = r.suggestions.iter().map(|s| s.kind.as_str()).collect();
        for k in ["slow_command", "error_loop", "reread", "permission"] {
            assert!(kinds.contains(&k), "{k} in {kinds:?}");
        }
        assert!(r.suggestions.len() <= 5);
        assert!(r.suggestions.iter().any(|s| s.text.contains("5 times and took 5m 10s")), "{:?}", r.suggestions);
        // Another session's prefix selects nothing.
        let none = build(&v, &wt, &Options { session: Some("zz".into()), ..opts() });
        assert!(none.sessions.is_empty());
        assert!(render(&none).contains("No sessions"));
        assert_eq!(dur(59), "59s");
        assert_eq!(dur(185), "3m 05s");
        assert_eq!(dur(1800), "30m");
        assert_eq!(dur(7620), "2h 07m");
    }
}
