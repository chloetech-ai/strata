//! Why a session ended, in plain words: its exit code (a signal is 128 +
//! its number), whether Stop was pressed here, Claude Code's own
//! `SessionEnd` reason, and the commands of other agents that ended
//! processes just before (`pkill -f serve.py` also ends every process
//! whose command line has "serve.py" in it, an agent's own among them:
//! its task is on its command line).

use chloedb::query::Params;
use chloedb::Value;
use serde_json::Value as Json;

use crate::db::{get_ms, get_str, ts};
use crate::supervisor::{one_line, Supervisor};

/// What Claude Code's `SessionEnd` reason means.
pub fn session_end_words(reason: &str) -> String {
    match reason {
        "" => "session ended".into(),
        "clear" => "session ended: /clear started a new one".into(),
        "logout" => "session ended: Claude Code logged out".into(),
        "prompt_input_exit" => "session ended: it was left with /exit or Ctrl-D".into(),
        "other" => "session ended: Claude Code was closed from outside it (a signal, not /exit)".into(),
        r => format!("session ended ({r})"),
    }
}

/// Whether a shell command ends processes: `kill`, `pkill`, `killall`,
/// `fuser -k`, `xargs kill`.
pub fn kills(cmd: &str) -> bool {
    let words: Vec<&str> = cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '`')).filter(|w| !w.is_empty()).collect();
    words.iter().enumerate().any(|(i, w)| {
        let b = w.rsplit('/').next().unwrap_or(w);
        matches!(b, "kill" | "pkill" | "killall") || (b == "fuser" && words[i + 1..].iter().any(|x| x.starts_with('-') && x.contains('k')))
    })
}

/// The commands that ended processes in the `window_ms` before `at_ms`,
/// by any agent but `id`: "flake-hunt ran `pkill -f serve.py` 2 s before".
pub fn kill_suspects(sup: &Supervisor, id: &str, at_ms: i64, window_ms: i64) -> Vec<String> {
    let p = Params::new().set("since", ts(at_ms - window_ms)).set("to", ts(at_ms + 1000)).set("id", Value::Str(id.to_string()));
    let rows = sup.db().query("SELECT agent, ts, data FROM events WHERE kind = 'PreToolUse' AND tool = 'Bash' AND ts >= :since AND ts <= :to AND agent != :id ORDER BY seq ASC LIMIT 2000", &p).unwrap_or_default();
    let mut out = Vec::new();
    for r in rows {
        let Some(data) = get_str(&r[2]).and_then(|d| serde_json::from_str::<Json>(&d).ok()) else { continue };
        let Some(cmd) = data["tool_input"]["command"].as_str() else { continue };
        if !kills(cmd) {
            continue;
        }
        let agent = get_str(&r[0]).unwrap_or_default();
        let name = sup.agent_row(&agent).map(|a| a.name).unwrap_or(agent);
        let before = (at_ms - get_ms(&r[1]).unwrap_or(at_ms)).max(0) / 1000;
        out.push(format!("{name} ran `{}` {before} s before", one_line(cmd, 120)));
    }
    out.truncate(3);
    out
}

/// Why a session ended, in a sentence or two. `stopped`: Stop was pressed
/// here; `end_reason`: its `SessionEnd` reason, when it sent one.
pub fn exit_words(code: i64, stopped: bool, end_reason: Option<&str>, suspects: &[String]) -> String {
    if stopped {
        return match code {
            137 | -1 => "Stopped from Workbench: it did not end within 5 s of Stop, so it was killed.".into(),
            _ => "Stopped from Workbench (Stop).".into(),
        };
    }
    let from_outside = |what: &str| {
        let mut s = format!("Something outside Workbench ended it: it got {what} and nobody pressed Stop here.");
        if suspects.is_empty() {
            s.push_str(" Often that is a `pkill -f` or `killall` whose pattern matched its command line (an agent stopping \"its\" server, say), in this Workbench or another program.");
        } else {
            s.push_str(&format!(" Just before, {}.", suspects.join("; ")));
        }
        s
    };
    match code {
        0 if end_reason == Some("prompt_input_exit") => "It ended normally: Claude Code was left with /exit or Ctrl-D.".into(),
        0 if end_reason == Some("other") => from_outside("a signal it handled (exit 0)"),
        0 => "It ended normally (exit 0).".into(),
        129 => "Its terminal was hung up (SIGHUP, exit 129), not by Stop here.".into(),
        130 => "It was interrupted (SIGINT, exit 130), not by Stop here.".into(),
        143 => from_outside("SIGTERM (exit 143)"),
        137 => from_outside("SIGKILL (exit 137: a `kill -9`, or the system ran out of memory)"),
        c if c > 128 && c < 160 => from_outside(&format!("signal {} (exit {c})", c - 128)),
        c => format!("Claude Code exited with code {c} by itself: its terminal's last lines say why."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_for_an_ending() {
        assert!(exit_words(129, true, None, &[]).starts_with("Stopped from Workbench"));
        let w = exit_words(143, false, Some("other"), &["paybutton ran `pkill -f \"python3 serve.py\"` 1 s before".into()]);
        assert!(w.contains("SIGTERM") && w.contains("nobody pressed Stop") && w.contains("paybutton ran"), "{w}");
        assert!(exit_words(143, false, None, &[]).contains("pkill -f"));
        assert_eq!(exit_words(0, false, Some("prompt_input_exit"), &[]), "It ended normally: Claude Code was left with /exit or Ctrl-D.");
        assert!(exit_words(1, false, None, &[]).contains("code 1"));
        assert!(kills("pkill -f \"python3 serve.py\"; sleep 1") && kills("lsof -ti :8417 | xargs kill") && kills("fuser -k 8417/tcp") && !kills("python3 -m pytest -k kill_switch"));
        assert!(session_end_words("other").contains("from outside"));
    }
}
