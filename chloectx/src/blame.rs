//! `chloectx blame path[:line]`: from a line of code to the session, the
//! prompt and the tool calls that wrote it, what memory had put in the
//! agent's context before it did, and whether that recall replays.
//!
//! The edit is found among the stored tool calls (`Edit`, `MultiEdit`,
//! `Write`) of the file: the newest that added the line (a whole line of
//! its new text, and not one of the text it replaced), else the newest
//! edit of the file. Its turn gives the prompt, the calls before it
//! and the notes distilled from the turn. The turn's receipts, and the
//! session's earlier ones still in context, give the records recalled, with
//! their scores; each is checked against the store as it is now (deleted,
//! superseded, from an untrusted source). The turn's prompt receipt is
//! replayed on its kept snapshot and on today's store
//! ([`crate::receipt::replay`]). When the project is a git repository, the
//! line's commit is shown too.

use std::path::Path;

use serde_json::{json, Value as Json};

use chloedb::query::Params;
use chloedb::Value;

use crate::receipt::{self, Book, Receipt, Replayed};
use crate::recall::fmt_time;
use crate::record::{Kind, Record, COLUMNS};
use crate::snapshots::Snapshots;
use crate::store::{row_record, Store};
use crate::{err, Result};

/// The turns before an edit's whose recalls blame shows.
pub const EARLIER_TURNS: i64 = 3;

/// Tools whose stored calls changed a file.
const EDIT_TOOLS: &str = "'Edit', 'MultiEdit', 'Write', 'NotebookEdit'";

/// Tags of records that came from outside the project and its people.
fn untrusted(r: &Record) -> Option<&'static str> {
    match (r.kind, r.tag.as_str()) {
        (Kind::ToolResult | Kind::Error, "WebFetch" | "WebSearch") => Some("untrusted source: the web"),
        (Kind::ToolResult, t) if t.starts_with("mcp__") => Some("from another tool's output"),
        (Kind::ToolResult, _) => Some("from a tool's output"),
        _ => None,
    }
}

/// The text an edit record says it wrote.
fn new_text(r: &Record) -> &str {
    if let Some(i) = r.text.find("\nwith\n") {
        return &r.text[i + 6..];
    }
    r.text.split_once('\n').map_or("", |x| x.1)
}

/// The text an edit record says it replaced (none for a `Write`).
fn old_text(r: &Record) -> &str {
    match (r.text.find(": replaced\n"), r.text.find("\nwith\n")) {
        (Some(a), Some(b)) if a + 11 <= b => &r.text[a + 11..b],
        _ => "",
    }
}

/// Whether `text` holds `line` as one of its lines, spaces at the ends aside.
fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|l| l.trim() == line)
}

/// Whether edit `r` added `line`: a line of what it wrote, and not of what
/// it replaced (an edit that only moved code around it did not write it).
pub fn added_line(r: &Record, line: &str) -> bool {
    has_line(new_text(r), line) && !has_line(old_text(r), line)
}

/// The path relative to the project, as stored edits name it. A relative
/// path is taken as relative to the project: the daemon's own working
/// directory is wherever the process that started it happened to be (the
/// CLI makes the path absolute against its own first).
pub fn relative(project: &str, path: &str) -> String {
    let p = Path::new(path);
    let abs = if p.is_absolute() { p.to_path_buf() } else { Path::new(project).join(p) };
    let abs = abs.canonicalize().unwrap_or(abs);
    let proj = Path::new(project).canonicalize().unwrap_or_else(|_| Path::new(project).to_path_buf());
    match abs.strip_prefix(&proj) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => path.trim_start_matches("./").to_string(),
    }
}

/// The most bytes of a file blame reads for its line.
pub const MAX_FILE_BYTES: u64 = 8 << 20;

/// The text of file `rel` of the project: a regular file inside it once
/// links and `..` are resolved, of at most [`MAX_FILE_BYTES`]. Blame is
/// asked for from the command center and over the daemon's socket, so it
/// must not read a key under `~/.ssh`, nor `/dev/zero` until memory ends.
pub fn read_project_file(project: &str, rel: &str) -> Result<String> {
    use std::io::Read;
    let proj = Path::new(project).canonicalize().map_err(|e| err(format!("project {project}: {e}")))?;
    let path = proj.join(rel).canonicalize().map_err(|e| err(format!("read {rel}: {e}")))?;
    if !path.starts_with(&proj) {
        return Err(err(format!("{rel} is not a file of the project {}", proj.display())));
    }
    let file = std::fs::File::open(&path).map_err(|e| err(format!("read {rel}: {e}")))?;
    let meta = file.metadata().map_err(|e| err(format!("read {rel}: {e}")))?;
    if !meta.is_file() {
        return Err(err(format!("{rel} is not a regular file")));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(err(format!("{rel} is {} bytes; blame reads files of at most {MAX_FILE_BYTES}", meta.len())));
    }
    let mut body = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut body).map_err(|e| err(format!("read {rel}: {e}")))?;
    if body.len() as u64 > MAX_FILE_BYTES {
        return Err(err(format!("{rel} is over {MAX_FILE_BYTES} bytes; blame reads files of at most that")));
    }
    String::from_utf8(body).map_err(|_| err(format!("read {rel}: not UTF-8 text")))
}

/// `path:line` or `path`.
pub fn parse_target(t: &str) -> (String, Option<usize>) {
    match t.rsplit_once(':') {
        Some((p, l)) if !p.is_empty() && l.parse::<usize>().is_ok_and(|n| n > 0) => (p.to_string(), l.parse().ok()),
        _ => (t.to_string(), None),
    }
}

fn git_line(project: &str, rel: &str, line: usize) -> Option<Json> {
    let out = std::process::Command::new("git").args(["-C", project, "blame", "--porcelain", "-L", &format!("{line},{line}"), "--", rel]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let sha = s.split_whitespace().next()?.to_string();
    let field = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
    Some(json!({
        "commit": sha,
        "uncommitted": sha.chars().all(|c| c == '0'),
        "author": field("author "),
        "time": field("author-time ").and_then(|t| t.parse::<i64>().ok()),
        "summary": field("summary "),
    }))
}

fn records(store: &Store, sql: &str, p: &Params) -> Result<Vec<Record>> {
    Ok(store.query_with(sql, p)?.rows.iter().filter_map(|r| row_record(r)).collect())
}

fn one_line(r: &Record, n: usize) -> String {
    let t: String = r.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = t.chars().take(n).collect();
    if t.chars().count() > n {
        s.push('…');
    }
    s
}

/// What blame found, as JSON with a `text` for a person.
pub fn blame(store: &Store, book: Option<&Book>, snaps: &Snapshots, project: &str, target: &str, replay: bool) -> Result<Json> {
    let (path, line) = parse_target(target);
    let rel = relative(project, &path);
    let line_text = match line {
        Some(n) => {
            let body = read_project_file(project, &rel)?;
            Some(body.lines().nth(n - 1).ok_or_else(|| err(format!("{rel} has {} lines", body.lines().count())))?.trim().to_string())
        }
        None => None,
    };
    let store = store.pinned();
    let terms = chloedb::text::analyze(&rel);
    if terms.is_empty() {
        return Err(err(format!("{rel} names no file")));
    }
    let files = Params::new().set("files", Value::Str(terms.join(" AND ")));
    let edits: Vec<Record> = records(&store, &format!("SELECT {COLUMNS} FROM memory WHERE kind = 'tool_call' AND tag IN ({EDIT_TOOLS}) AND MATCH(files, :files) ORDER BY seq DESC LIMIT 500"), &files)?
        .into_iter()
        .filter(|r| r.files.split_whitespace().any(|f| f == rel) || r.text.split_whitespace().nth(1).is_some_and(|p| p.trim_end_matches(':') == rel))
        .collect();
    let git = line.and_then(|n| git_line(project, &rel, n));
    let wanted = line_text.as_deref().filter(|t| t.chars().count() >= 2);
    let (edit, how) = match wanted.and_then(|t| edits.iter().find(|e| added_line(e, t))) {
        Some(e) => (Some(e), "the newest recorded edit that added the line"),
        None => match (wanted, git.as_ref().and_then(|g| g["time"].as_i64()).filter(|_| git.as_ref().is_some_and(|g| g["uncommitted"] != true))) {
            (Some(_), Some(t)) => (edits.iter().find(|e| e.ts <= t + 60).or(edits.first()), "no recorded edit holds the line's text; the newest edit of the file before its commit"),
            (Some(_), None) => (edits.first(), "no recorded edit holds the line's text; the newest recorded edit of the file"),
            (None, _) => (edits.first(), "the newest recorded edit of the file"),
        },
    };
    let mut text = format!("$ chloectx blame {rel}{}\n", line.map_or(String::new(), |n| format!(":{n}")));
    if let Some(t) = &line_text {
        text += &format!("line        {}\n", t.chars().take(110).collect::<String>());
    }
    if let Some(g) = &git {
        if g["uncommitted"] == true {
            text += "commit      not committed yet\n";
        } else {
            text += &format!(
                "commit      {} {} · {}\n",
                g["commit"].as_str().unwrap_or("").chars().take(10).collect::<String>(),
                g["time"].as_i64().map(fmt_time).unwrap_or_default(),
                g["summary"].as_str().unwrap_or("")
            );
        }
    }
    let Some(edit) = edit else {
        text += &format!("\nChloeCtx recorded no edit of {rel}: it was written outside a recorded session, or before ChloeCtx was installed.\n");
        return Ok(json!({"text": text, "file": rel, "line": line, "git": git, "edit": null, "edits": 0}));
    };
    let sp = Params::new().set("s", Value::Str(edit.session.clone()));
    let turn: Vec<Record> = records(&store, &format!("SELECT {COLUMNS} FROM memory WHERE session = :s AND turn = {} ORDER BY seq ASC LIMIT 1000", edit.turn), &sp)?;
    let prompt = turn.iter().find(|r| r.kind == Kind::Prompt);
    let calls: Vec<&Record> = turn.iter().filter(|r| r.kind == Kind::ToolCall && r.seq < edit.seq).collect();
    let notes: Vec<&Record> = turn.iter().filter(|r| r.kind.is_knowledge() && r.kind != Kind::Todo).collect();
    text += &format!("edit        {} · session {} turn {} · {} ({how})\n", fmt_time(edit.ts), edit.session.chars().take(8).collect::<String>(), edit.turn, edit.id);
    if let Some(p) = prompt {
        text += &format!("prompt      {}\n", one_line(p, 140));
    }
    if !calls.is_empty() {
        text += &format!("before it   {} tool call{} in the turn:\n", calls.len(), if calls.len() == 1 { "" } else { "s" });
        for c in calls.iter().rev().take(8).rev() {
            text += &format!("              {}\n", one_line(c, 100));
        }
    }
    for n in &notes {
        text += &format!("{:<11} {} ({})\n", n.kind.as_str(), one_line(n, 120), n.id);
    }

    // The receipts of recalls before the edit, in its session: its turn's
    // and up to EARLIER_TURNS turns before it, read by turn (a session's
    // oldest receipts say nothing about a late edit).
    let receipts: Vec<Receipt> = match book {
        Some(b) => b.of_turns(&edit.session, edit.turn - EARLIER_TURNS, edit.turn)?.into_iter().filter(|r| r.snapshot.last_seq < edit.seq && !r.shown.is_empty()).collect(),
        None => Vec::new(),
    };
    let this_turn: Vec<&Receipt> = receipts.iter().filter(|r| r.turn == edit.turn).collect();
    let earlier: Vec<&Receipt> = receipts.iter().filter(|r| r.turn < edit.turn).rev().collect();
    // Every row a receipt below shows (`Row::shown`: listed rows too, not
    // only `Receipt::shown`, the ones shown in full), so that a row missing
    // from the store now is one that was deleted (CTX-6).
    let mut shown_ids: Vec<String> = this_turn.iter().chain(earlier.iter()).flat_map(|r| r.rows.iter().filter(|x| x.shown()).map(|x| x.id.clone()).chain(r.shown.iter().cloned())).collect();
    shown_ids.sort();
    shown_ids.dedup();
    let now_recs = store.get(&shown_ids)?;
    let status = |id: &str| -> Vec<String> {
        let mut flags = Vec::new();
        match now_recs.iter().find(|r| r.id == id) {
            None => flags.push("deleted since".into()),
            Some(r) => {
                if r.tag == "superseded" {
                    match r.linked("superseded_by").next() {
                        Some(by) => flags.push(format!("superseded since, by {by}")),
                        None => flags.push("superseded since".into()),
                    }
                }
                if let Some(u) = untrusted(r) {
                    flags.push(u.into());
                }
            }
        }
        flags
    };
    let mut causes: Vec<String> = Vec::new();
    let mut rec_json = Vec::new();
    let mut show = |rc: &Receipt, text: &mut String, label: &str| {
        *text += &format!(
            "\n{label}: receipt {} · {} · {} · store commit {}\n",
            rc.id,
            rc.source,
            rc.decision.as_deref().unwrap_or(match rc.source.as_str() { "briefing" => "shown whole", _ => "listed" }),
            rc.snapshot.txn
        );
        *text += &format!("  {:>3} {:<11} {:<26} {:>6} {:>6} {:>5}  {}\n", "#", "kind", "record", "score", "cos", "tok", "decision");
        for row in rc.rows.iter().filter(|r| r.shown()) {
            let flags = status(&row.id);
            for f in &flags {
                if f.starts_with("superseded") || f.starts_with("deleted") || f.starts_with("untrusted") {
                    causes.push(format!("{} ({}) was in context before the edit and is {f}", row.id, row.kind));
                }
            }
            *text += &format!(
                "  {:>3} {:<11} {:<26} {:>6.3} {:>6} {:>5}  {}{}\n",
                if row.rank == 0 { "k".into() } else { row.rank.to_string() },
                row.kind,
                row.id,
                row.score,
                row.cos.map_or("-".into(), |c| format!("{c:.3}")),
                row.tokens,
                row.decision,
                if flags.is_empty() { String::new() } else { format!("  ← {}", flags.join(", ")) }
            );
        }
        rec_json.push(json!({"receipt": rc.id, "turn": rc.turn, "source": rc.source, "rows": rc.rows.iter().filter(|r| r.shown()).map(|r| json!({"row": r, "now": status(&r.id), "text": now_recs.iter().find(|x| x.id == r.id).map(|x| one_line(x, 160))})).collect::<Vec<_>>()}));
    };
    if book.is_none() {
        text += "\nreceipts are off for this store, so what memory recalled before the edit is unknown\n";
    } else if this_turn.is_empty() && earlier.is_empty() {
        text += "\nmemory recalled nothing into the session before the edit (no receipt shows a record)\n";
    }
    for rc in &this_turn {
        show(rc, &mut text, "recalled in the edit's turn");
    }
    for rc in &earlier {
        show(rc, &mut text, &format!("recalled earlier in the session (turn {})", rc.turn));
    }

    let mut replays: Vec<Replayed> = Vec::new();
    if replay {
        if let Some(rc) = this_turn.iter().find(|r| r.source != "search").or(this_turn.first()).or(earlier.first()) {
            replays = receipt::replay(rc, snaps, &store)?;
            text += &format!("\nreplay of {}:\n", rc.id);
            text += &fmt_replays(&replays);
            for r in &replays {
                if r.on == "now" && !r.verdict.same_shown {
                    causes.push("the same request today recalls different records".into());
                }
            }
        }
    }
    causes.dedup();
    if !causes.is_empty() {
        text += "\nworth a look:\n";
        for c in &causes {
            text += &format!("  - {c}\n");
        }
    }
    Ok(json!({
        "text": text, "file": rel, "line": line, "line_text": line_text, "git": git, "how": how,
        "edit": edit, "prompt": prompt, "calls": calls, "notes": notes,
        "receipts": rec_json, "replays": replays, "causes": causes, "edits": edits.len(),
    }))
}

/// Replays for a person: one line a version, and what changed.
pub fn fmt_replays(rs: &[Replayed]) -> String {
    let mut s = String::new();
    for r in rs {
        let on = match (&r.snapshot, r.on.as_str()) {
            (Some(k), "kept snapshot") => format!("on the snapshot kept at commit {} ({})", r.txn, k.name),
            (Some(k), _) => format!("on the nearest later snapshot, commit {} ({}; none was kept at the receipt's)", k.at, k.name),
            _ => format!("on today's store (commit {})", r.txn),
        };
        let v = &r.verdict;
        let what = if v.identical {
            "identical: the same rows, scores and decisions".to_string()
        } else if v.same_shown {
            "the same records shown, with the same decisions; scores or lower ranks moved".to_string()
        } else {
            "different".to_string()
        };
        s += &format!("  {on}: {what}\n");
        if !v.identical {
            for c in &v.changes {
                s += &format!("      {c}\n");
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blame_reads_only_regular_files_of_the_project_under_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("secret"), "key\n").unwrap();
        let p = project.to_str().unwrap();
        assert_eq!(read_project_file(p, "src/a.rs").unwrap(), "fn a() {}\n");
        assert_eq!(read_project_file(p, "./src/../src/a.rs").unwrap(), "fn a() {}\n");
        // Out of the project: by `..`, by an absolute path, by a link.
        for rel in ["../secret", dir.path().join("secret").to_str().unwrap(), "/etc/hostname"] {
            let e = read_project_file(p, rel).unwrap_err().to_string();
            assert!(e.contains("not a file of the project") || e.contains("No such file"), "{rel}: {e}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("secret"), project.join("link")).unwrap();
            let e = read_project_file(p, "link").unwrap_err().to_string();
            assert!(e.contains("not a file of the project"), "{e}");
            std::os::unix::fs::symlink("/dev/zero", project.join("zero")).unwrap();
            assert!(read_project_file(p, "zero").is_err());
        }
        assert!(read_project_file(p, "/dev/zero").is_err());
        // A directory, and a file past the cap.
        assert!(read_project_file(p, "src").unwrap_err().to_string().contains("not a regular file"));
        let big = std::fs::File::create(project.join("big")).unwrap();
        big.set_len(MAX_FILE_BYTES + 1).unwrap();
        assert!(read_project_file(p, "big").unwrap_err().to_string().contains("at most"));
    }
}
