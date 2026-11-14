//! A `pkill -f "python3 serve.py"` that would end processes the agent did
//! not start: every process whose command line holds the pattern, another
//! agent's Claude Code among them (its task is on its command line), and
//! another agent's server. Workbench's PreToolUse hook denies such a call
//! (src/guarded.rs) and says which processes it would have ended and how
//! to end only the agent's own (`kill <pid>`).

use std::collections::HashMap;

use crate::supervisor::{one_line, Supervisor};
use crate::testruns::simple_commands;

/// A process: its parent and command line (`comm` when it has none).
#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub cmdline: String,
}

/// Every process now (Linux; none elsewhere, and the guard says nothing).
pub fn processes() -> Vec<Proc> {
    #[allow(unused_mut)] // filled on Linux only
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { continue };
            let comm = stat[open + 1..close].to_string();
            let ppid = stat[close + 1..].split_whitespace().nth(1).and_then(|p| p.parse().ok()).unwrap_or(0);
            let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let cmdline = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect::<Vec<_>>().join(" ");
            out.push(Proc { pid, ppid, comm, cmdline });
        }
    }
    out
}

/// What one `pkill`/`killall` (or `kill $(pgrep …)`) matches: the pattern,
/// against the whole command line (`-f`) or the process name, exactly
/// (`-x`, killall) or anywhere.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub pattern: String,
    pub full: bool,
    pub exact: bool,
}

/// The process patterns a command ends processes by.
pub fn targets(cmd: &str) -> Vec<Target> {
    let kill_word = crate::endings::kills(cmd);
    let mut out = Vec::new();
    let cmds = simple_commands(cmd);
    // `kill $(pgrep -f x)`: the command inside the substitution too.
    for c in &cmds {
        for w in &c.words {
            let inner = w.find("$(").map(|i| &w[i + 2..]).or_else(|| w.find('`').map(|i| &w[i + 1..]));
            if let Some(inner) = inner {
                let inner = inner.trim_end_matches([')', '`']);
                if kill_word && inner.contains("pgrep") {
                    out.extend(targets(&format!("{inner}; kill")));
                }
            }
        }
    }
    for c in cmds {
        let mut w: &[String] = &c.words;
        while w.first().is_some_and(|x| matches!(x.as_str(), "sudo" | "xargs")) {
            w = &w[1..];
        }
        let Some(first) = w.first() else { continue };
        let prog = first.rsplit('/').next().unwrap_or(first);
        let pkill_like = prog == "pkill" || (prog == "pgrep" && kill_word);
        if pkill_like {
            let (mut full, mut exact) = (false, false);
            let mut i = 1;
            let mut pattern = None;
            while i < w.len() {
                let a = &w[i];
                if a == "--full" {
                    full = true;
                } else if a == "--exact" {
                    exact = true;
                } else if matches!(a.as_str(), "-u" | "-U" | "-g" | "-G" | "-P" | "-s" | "-t" | "-F" | "--signal" | "--euid" | "--uid" | "--parent" | "--pidfile" | "--session" | "--terminal" | "-d" | "--delimiter") {
                    i += 1;
                } else if a.starts_with("--") {
                } else if let Some(flags) = a.strip_prefix('-') {
                    if flags.chars().all(|ch| ch.is_ascii_alphabetic()) && !flags.chars().all(|ch| ch.is_ascii_uppercase()) {
                        full |= flags.contains('f');
                        exact |= flags.contains('x');
                    }
                } else {
                    pattern = Some(a.clone());
                    break;
                }
                i += 1;
            }
            if let Some(p) = pattern {
                out.push(Target { pattern: p, full, exact });
            }
        } else if prog == "killall" {
            let mut i = 1;
            while i < w.len() {
                let a = &w[i];
                if matches!(a.as_str(), "-s" | "--signal" | "-u" | "--user" | "-o" | "-y") {
                    i += 1;
                } else if !a.starts_with('-') {
                    out.push(Target { pattern: a.clone(), full: false, exact: true });
                }
                i += 1;
            }
        }
    }
    out
}

/// A small regular expression: literals, `.`, `*`, `+`, `?`, `^`, `$`,
/// `\x` and `[…]`. `None` for what it does not read (groups, `|`, `{}`):
/// then the guard says nothing.
pub fn re_find(pat: &str, text: &str, exact: bool) -> Option<bool> {
    if pat.contains(['(', ')', '|', '{', '}']) {
        return None;
    }
    #[derive(Clone)]
    enum Atom {
        Any,
        Ch(char),
        Set(Vec<(char, char)>, bool),
    }
    let mut atoms: Vec<(Atom, char)> = Vec::new(); // (atom, repeat: '1', '*', '+', '?')
    let p: Vec<char> = pat.chars().collect();
    let (mut start, mut end) = (exact, exact);
    let mut i = 0;
    if p.first() == Some(&'^') {
        start = true;
        i = 1;
    }
    while i < p.len() {
        let a = match p[i] {
            '$' if i + 1 == p.len() => {
                end = true;
                i += 1;
                continue;
            }
            '.' => Atom::Any,
            '\\' if i + 1 < p.len() => {
                i += 1;
                Atom::Ch(p[i])
            }
            '[' => {
                let mut j = i + 1;
                let neg = p.get(j) == Some(&'^');
                if neg {
                    j += 1;
                }
                let mut set = Vec::new();
                while j < p.len() && (p[j] != ']' || set.is_empty() && j == i + 1 + neg as usize) {
                    if j + 2 < p.len() && p[j + 1] == '-' && p[j + 2] != ']' {
                        set.push((p[j], p[j + 2]));
                        j += 3;
                    } else {
                        set.push((p[j], p[j]));
                        j += 1;
                    }
                }
                if j >= p.len() {
                    return None;
                }
                i = j;
                Atom::Set(set, neg)
            }
            c => Atom::Ch(c),
        };
        i += 1;
        let rep = match p.get(i) {
            Some(r @ ('*' | '+' | '?')) => {
                i += 1;
                *r
            }
            _ => '1',
        };
        atoms.push((a, rep));
    }
    fn one(a: &Atom, c: char) -> bool {
        match a {
            Atom::Any => true,
            Atom::Ch(x) => *x == c,
            Atom::Set(s, neg) => s.iter().any(|(lo, hi)| (*lo..=*hi).contains(&c)) != *neg,
        }
    }
    fn at(atoms: &[(Atom, char)], t: &[char], end: bool) -> bool {
        let Some(((a, rep), rest)) = atoms.split_first() else { return !end || t.is_empty() };
        match rep {
            '1' => !t.is_empty() && one(a, t[0]) && at(rest, &t[1..], end),
            '?' => (!t.is_empty() && one(a, t[0]) && at(rest, &t[1..], end)) || at(rest, t, end),
            _ => {
                let min = usize::from(*rep == '+');
                let mut n = 0;
                while n < t.len() && one(a, t[n]) {
                    n += 1;
                }
                (min..=n).rev().any(|k| at(rest, &t[k..], end))
            }
        }
    }
    let t: Vec<char> = text.chars().collect();
    if start {
        return Some(at(&atoms, &t, end));
    }
    Some((0..=t.len()).any(|s| at(&atoms, &t[s..], end)))
}

/// The processes a command would end that are not `own`'s (its process
/// and its descendants) nor this server; `None` when it ends none such or
/// the guard cannot tell.
pub fn others_hit(cmd: &str, own: i32, procs: &[Proc]) -> Vec<Proc> {
    let ts = targets(cmd);
    if ts.is_empty() || own <= 1 {
        return Vec::new();
    }
    let parent: HashMap<i32, i32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let mine = |mut pid: i32| {
        for _ in 0..64 {
            if pid == own {
                return true;
            }
            match parent.get(&pid) {
                Some(&pp) if pp > 1 && pp != pid => pid = pp,
                _ => return false,
            }
        }
        false
    };
    let me = std::process::id() as i32;
    let mut out = Vec::new();
    for p in procs {
        if mine(p.pid) || p.pid == me || p.cmdline.is_empty() && p.comm.is_empty() {
            continue;
        }
        let hit = ts.iter().any(|t| {
            let text = if t.full { p.cmdline.as_str() } else { p.comm.as_str() };
            let name = p.cmdline.split_whitespace().next().map(|a| a.rsplit('/').next().unwrap_or(a)).unwrap_or("");
            re_find(&t.pattern, text, t.exact).unwrap_or(false) || (!t.full && t.exact && name == t.pattern)
        });
        if hit {
            out.push(p.clone());
        }
    }
    out
}

/// The deny reason for an agent's command that would end other processes.
pub fn check(sup: &Supervisor, id: &str, own_pid: Option<i64>, cmd: &str) -> Option<String> {
    let own = own_pid? as i32;
    if targets(cmd).is_empty() {
        return None;
    }
    let procs = processes();
    let hit = others_hit(cmd, own, &procs);
    if hit.is_empty() {
        return None;
    }
    // Name the agents the processes belong to.
    let parent: HashMap<i32, i32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let agents: Vec<(i32, String)> = sup.agent_rows().into_iter().filter(|a| a.id != id && sup.is_running(&a.id)).filter_map(|a| Some((a.pid? as i32, a.name))).collect();
    let owner = |mut pid: i32| -> Option<String> {
        for _ in 0..64 {
            if let Some((_, n)) = agents.iter().find(|(p, _)| *p == pid) {
                return Some(n.clone());
            }
            pid = *parent.get(&pid)?;
        }
        None
    };
    let list: Vec<String> = hit
        .iter()
        .take(3)
        .map(|p| {
            let who = owner(p.pid).map(|n| format!(", agent {n}'s")).unwrap_or_default();
            format!("pid {}{who}: {}", p.pid, one_line(if p.cmdline.is_empty() { &p.comm } else { &p.cmdline }, 90))
        })
        .collect();
    Some(format!(
        "Workbench stops this: `{}` would also end {} process{} this session did not start ({}{}). End only your own: `kill <pid>` with the pid of the process you started (`jobs -l`, or `lsof -ti :PORT` for a server on PORT).",
        one_line(cmd, 120),
        hit.len(),
        if hit.len() == 1 { "" } else { "es" },
        list.join("; "),
        if hit.len() > 3 { ", …" } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, comm: &str, cmdline: &str) -> Proc {
        Proc { pid, ppid, comm: comm.into(), cmdline: cmdline.into() }
    }

    #[test]
    fn a_pkill_that_would_end_another_agent() {
        assert_eq!(targets("pkill -f \"python3 serve.py\"; sleep 1"), [Target { pattern: "python3 serve.py".into(), full: true, exact: false }]);
        assert_eq!(targets("pkill -9 -f serve"), [Target { pattern: "serve".into(), full: true, exact: false }]);
        assert_eq!(targets("killall node")[0], Target { pattern: "node".into(), full: false, exact: true });
        assert_eq!(targets("kill $(pgrep -f serve.py)")[0].pattern, "serve.py");
        assert!(targets("python3 -m pytest -k kill").is_empty() && targets("pgrep -f serve").is_empty());
        assert_eq!(re_find("serve.py", "python3 serve.py", false), Some(true));
        assert_eq!(re_find("^python3 s.*py$", "python3 serve.py", false), Some(true));
        assert_eq!(re_find("node", "nodejs", true), Some(false));
        assert_eq!(re_find("a(b|c)", "ab", false), None);
        assert_eq!(re_find("[0-9]+ passed", "8 passed", false), Some(true));
        // Agent 100 (its own server 101), agent 200 whose task names the
        // server, and 201, agent 200's server.
        let procs = [
            p(100, 1, "claude", "claude --session-id x -- fix the page"),
            p(101, 100, "python3", "python3 serve.py"),
            p(200, 1, "claude", "claude --session-id y -- Start PORT=8417 python3 serve.py and set the preview"),
            p(201, 200, "python3", "python3 serve.py"),
        ];
        let hit: Vec<i32> = others_hit("pkill -f \"python3 serve.py\"", 100, &procs).iter().map(|p| p.pid).collect();
        assert_eq!(hit, [200, 201]);
        // Its own server by pid, or a pattern only it matches: allowed.
        assert!(others_hit("kill 101", 100, &procs).is_empty());
        assert!(others_hit("pkill -f 'fix the page'", 200, &procs).len() == 1);
        assert!(others_hit("pkill -f nothing-like-it", 100, &procs).is_empty());
    }
}
