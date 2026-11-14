//! Servers the agents started (SPEC.md, "Sixth build: E"): a dev server an
//! agent runs (`python3 serve.py`, `npm run dev`) outlives its Claude Code
//! session, since a background command leaves the session's process
//! group. Workbench finds them as the processes that listen on a TCP port
//! and carry the agent's `CHLOEDB_WORKBENCH_AGENT` in their environment
//! (every command of a session inherits it), or descend from the agent's
//! process; lists them (`GET /api/servers`), stops one
//! (`POST /api/servers/:pid/stop`), and stops an agent's when the agent is
//! stopped: as the stop request says, else as the user chose once
//! (`PUT /api/servers/policy`, the mark `servers:on-stop`), else yes.
//!
//! On Linux it reads `/proc`; elsewhere `lsof` and `ps`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json as AxJson, Router};
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};

const POLICY: &str = "servers:on-stop";

/// A process an agent started that listens on a port.
#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub pid: i32,
    pub agent: String,
    pub command: String,
    pub ports: Vec<u16>,
    pub since_ms: Option<i64>,
    pub cwd: Option<String>,
}

impl Server {
    pub fn to_json(&self, sup: &Supervisor) -> Json {
        let a = sup.agent_row(&self.agent);
        json!({
            "pid": self.pid, "agent": self.agent, "name": a.as_ref().map(|a| a.name.clone()),
            "agent_running": sup.is_running(&self.agent),
            "command": self.command, "ports": self.ports, "port": self.ports.first(),
            "since_ms": self.since_ms, "cwd": self.cwd,
            "preview": a.as_ref().and_then(|a| a.preview_url.clone()).filter(|u| self.ports.iter().any(|p| u.contains(&format!(":{p}")))),
        })
    }
}

/// The user's choice for an agent's servers when it stops: `stop`, `keep`,
/// or `None` (not asked yet).
pub fn policy(sup: &Supervisor) -> Option<String> {
    sup.mark(POLICY).filter(|v| v == "stop" || v == "keep")
}

/// Every agent's servers now (the agents Workbench knows).
pub fn scan(sup: &Supervisor) -> Vec<Server> {
    let agents = sup.agent_rows();
    let known: HashMap<String, Option<i32>> = agents.iter().map(|a| (a.id.clone(), a.pid.map(|p| p as i32).filter(|_| sup.is_running(&a.id)))).collect();
    let mut out = listeners()
        .into_iter()
        .filter(|l| l.pid != std::process::id() as i32)
        .filter_map(|l| {
            // Its agent: by the environment, else by ancestry to a running
            // agent's process (never the agent's own process).
            let by_env = l.env_agent.clone().filter(|a| known.contains_key(a));
            let agent = by_env.or_else(|| {
                let roots: HashMap<i32, &String> = known.iter().filter_map(|(id, p)| p.map(|p| (p, id))).collect();
                l.ancestors.iter().find_map(|p| roots.get(p).map(|id| (*id).clone()))
            })?;
            if known.get(&agent).copied().flatten() == Some(l.pid) {
                return None;
            }
            Some(Server { pid: l.pid, agent, command: l.command, ports: l.ports, since_ms: l.since_ms, cwd: l.cwd })
        })
        .collect::<Vec<_>>();
    out.sort_by_key(|s| (s.since_ms.unwrap_or(0), s.pid));
    out
}

/// The processes that listen on a port with `dir` as their working
/// directory (a dev server started in a project's main checkout, by
/// anyone): `(pid, command, ports)`, Workbench's own left out.
pub fn in_dir(dir: &std::path::Path) -> Vec<(i32, String, Vec<u16>)> {
    let want = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut out: Vec<(i32, String, Vec<u16>)> = listeners()
        .into_iter()
        .filter(|l| l.pid != std::process::id() as i32)
        .filter(|l| l.cwd.as_deref().is_some_and(|c| std::path::Path::new(c).canonicalize().map(|c| c == want).unwrap_or(false)))
        .map(|l| (l.pid, l.command, l.ports))
        .collect();
    out.sort_by_key(|x| x.0);
    out
}

/// One agent's servers.
pub fn of_agent(sup: &Supervisor, id: &str) -> Vec<Server> {
    scan(sup).into_iter().filter(|s| s.agent == id).collect()
}

/// Stops an agent's servers off the caller's thread, and records it.
pub fn stop_of_agent_later(sup: &Supervisor, id: &str) {
    let list = of_agent(sup, id);
    if list.is_empty() {
        return;
    }
    let what: Vec<String> = list.iter().map(|s| format!("{} (:{})", one_line(&s.command, 80), s.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", :"))).collect();
    let _ = sup.note(id, "servers_stopped", format!("stopped its server{}: {}", if list.len() > 1 { "s" } else { "" }, what.join("; ")), Some(json!({ "pids": list.iter().map(|s| s.pid).collect::<Vec<_>>() }).to_string()), |_| {});
    std::thread::spawn(move || {
        for s in &list {
            terminate(s.pid);
        }
    });
}

/// SIGTERM to the process (its group, when it leads one), SIGKILL after 3 s.
pub fn terminate(pid: i32) {
    if pid <= 1 {
        return;
    }
    let group = unsafe { libc::getpgid(pid) } == pid;
    let sig = |s: i32| unsafe {
        if group {
            libc::kill(-pid, s);
        }
        libc::kill(pid, s);
    };
    sig(libc::SIGTERM);
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && alive(pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if alive(pid) {
        sig(libc::SIGKILL);
    }
}

fn alive(pid: i32) -> bool {
    // A zombie is gone for our purpose: reaped by whoever is its parent.
    #[cfg(target_os = "linux")]
    if let Ok(st) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        if st.rsplit(')').next().is_some_and(|r| r.trim_start().starts_with('Z')) {
            return false;
        }
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A process that listens on a TCP port.
#[derive(Debug, Default)]
struct Listener {
    pid: i32,
    ports: Vec<u16>,
    command: String,
    env_agent: Option<String>,
    ancestors: Vec<i32>,
    since_ms: Option<i64>,
    cwd: Option<String>,
}

#[cfg(target_os = "linux")]
fn listeners() -> Vec<Listener> {
    use std::fs;
    // Listening sockets: inode → port.
    let mut inodes: HashMap<u64, u16> = HashMap::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in fs::read_to_string(f).unwrap_or_default().lines().skip(1) {
            let w: Vec<&str> = line.split_whitespace().collect();
            if w.len() > 9 && w[3] == "0A" {
                let port = w[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok());
                if let (Some(port), Ok(inode)) = (port, w[9].parse::<u64>()) {
                    inodes.insert(inode, port);
                }
            }
        }
    }
    if inodes.is_empty() {
        return Vec::new();
    }
    let btime: i64 = fs::read_to_string("/proc/stat").ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok()))).unwrap_or(0);
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as i64;
    let stat = |pid: i32| -> Option<(i32, i64)> {
        let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest: Vec<&str> = s.rsplit_once(')')?.1.split_whitespace().collect();
        Some((rest.get(1)?.parse().ok()?, rest.get(19)?.parse().ok()?))
    };
    let mut out = Vec::new();
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        let mut ports: Vec<u16> = fs::read_dir(format!("/proc/{pid}/fd"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|fd| fs::read_link(fd.path()).ok())
            .filter_map(|l| l.to_str().and_then(|s| s.strip_prefix("socket:[")).and_then(|s| s.strip_suffix(']')).and_then(|n| n.parse::<u64>().ok()))
            .filter_map(|i| inodes.get(&i).copied())
            .collect();
        if ports.is_empty() {
            continue;
        }
        ports.sort_unstable();
        ports.dedup();
        let env = fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        let env_agent = env.split(|b| *b == 0).find_map(|kv| kv.strip_prefix(b"CHLOEDB_WORKBENCH_AGENT=")).map(|v| String::from_utf8_lossy(v).to_string());
        let command = String::from_utf8_lossy(&fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default()).replace('\0', " ").trim().to_string();
        let mut ancestors = Vec::new();
        let mut p = pid;
        let mut since_ms = None;
        for i in 0..64 {
            let Some((pp, start)) = stat(p) else { break };
            if i == 0 && btime > 0 {
                since_ms = Some((btime * 1000) + start * 1000 / tck);
            }
            if pp <= 1 {
                break;
            }
            ancestors.push(pp);
            p = pp;
        }
        let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().to_string());
        out.push(Listener { pid, ports, command: one_line(&command, 300), env_agent, ancestors, since_ms, cwd });
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn listeners() -> Vec<Listener> {
    use std::process::Command;
    let Ok(o) = Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn"]).output() else { return Vec::new() };
    let mut by_pid: HashMap<i32, Vec<u16>> = HashMap::new();
    let mut cur = 0;
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        if let Some(p) = l.strip_prefix('p') {
            cur = p.parse().unwrap_or(0);
        } else if let Some(n) = l.strip_prefix('n') {
            if let Some(port) = n.rsplit(':').next().and_then(|p| p.parse().ok()) {
                by_pid.entry(cur).or_default().push(port);
            }
        }
    }
    let ps = |args: &[&str]| Command::new("ps").args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    by_pid
        .into_iter()
        .filter(|(p, _)| *p > 0)
        .map(|(pid, mut ports)| {
            ports.sort_unstable();
            ports.dedup();
            let p = pid.to_string();
            let command = ps(&["-o", "command=", "-p", &p]);
            let env = ps(&["eww", "-o", "command=", "-p", &p]);
            let env_agent = env.split_whitespace().find_map(|w| w.strip_prefix("CHLOEDB_WORKBENCH_AGENT=")).map(str::to_string);
            let mut ancestors = Vec::new();
            let mut cur = pid;
            for _ in 0..64 {
                let Ok(pp) = ps(&["-o", "ppid=", "-p", &cur.to_string()]).parse::<i32>() else { break };
                if pp <= 1 {
                    break;
                }
                ancestors.push(pp);
                cur = pp;
            }
            Listener { pid, ports, command: one_line(&command, 300), env_agent, ancestors, since_ms: None, cwd: None }
        })
        .collect()
}

// Routes.

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/servers", get(list))
        .route("/api/servers/policy", put(set_policy))
        .route("/api/servers/:pid/stop", post(stop_one))
}

#[derive(Deserialize)]
struct ListQ {
    agent: Option<String>,
}

async fn list(State(app): State<App>, Query(q): Query<ListQ>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        let all = scan(&sup);
        let list: Vec<Json> = all.iter().filter(|s| q.agent.as_deref().filter(|a| !a.is_empty()).is_none_or(|a| s.agent == a)).map(|s| s.to_json(&sup)).collect();
        Ok(json!({ "servers": list, "policy": policy(&sup) }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

#[derive(Deserialize)]
struct PolicyB {
    on_stop: Option<String>,
}

async fn set_policy(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: PolicyB = parse_body(&body)?;
    let v = b.on_stop.unwrap_or_default();
    if !matches!(v.as_str(), "stop" | "keep" | "") {
        return Err(ApiError::bad("on_stop is stop, keep, or empty (ask again)"));
    }
    app.sup.set_mark(POLICY, &v)?;
    Ok(AxJson(json!({ "policy": policy(&app.sup) })).into_response())
}

async fn stop_one(State(app): State<App>, Path(pid): Path<i32>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        // Only a process found as an agent's server: this is no general kill.
        let s = scan(&sup).into_iter().find(|s| s.pid == pid).ok_or_else(|| ApiError::not_found(format!("no agent's server has pid {pid}")))?;
        let j = s.to_json(&sup);
        let _ = sup.note(&s.agent, "servers_stopped", format!("you stopped its server: {} (:{})", one_line(&s.command, 80), s.ports.first().copied().unwrap_or(0)), Some(json!({ "pids": [s.pid] }).to_string()), |_| {});
        terminate(s.pid);
        Ok(json!({ "ok": true, "stopped": j, "alive": alive(s.pid) }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_a_listening_child_by_its_environment() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        // This test process listens: it is found, with its port.
        let me = listeners().into_iter().find(|x| x.pid == std::process::id() as i32).expect("this process listens");
        assert!(me.ports.contains(&port), "{me:?}");
        assert!(me.since_ms.is_some_and(|t| t > 1_600_000_000_000));
        assert!(me.ancestors.iter().all(|p| *p > 1));
        drop(l);
    }
}
