//! `chloedb-workbench mcp`: an MCP server over stdio (JSON-RPC lines) with
//! the tools that let an agent start and steer other agents. It is a client
//! of the HTTP API; its caller is `$CHLOEDB_WORKBENCH_AGENT`, the parent of
//! what it spawns.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::hook::{agent as http_agent, Env};

const PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "Workbench runs Claude Code sessions side by side. spawn_agent starts another agent: a real, interactive Claude Code session in its own terminal and (by default) its own git worktree and branch, which the user watches and can type into. Each one costs tokens like any session, so start one only when the work splits into parts that can go on at once, and give it a task that stands alone: it does not see this conversation, so say what to do, where, how to tell it is done, and what to report. Then wait_agent waits for its turn to finish (or for it to ask something), read_agent reads what it did, send_to_agent answers it, and stop_agent ends it. When only the user can decide something, ask_human asks them in Workbench (a question with options) and waits for the answer: the user does not watch your terminal. A goal's outcomes are kept with add_outcome and update_outcome.";

/// The longest an `ask_human` waits for the answer.
const ASK_WAIT_S: u64 = 30 * 60;

fn tools() -> Value {
    json!([
        {
            "name": "spawn_agent",
            "description": "Start another Claude Code agent on this repository, in its own terminal (and by default its own git worktree on a new branch wb/<name>). It is a real session that the user watches; it costs tokens. Give it a task that stands alone. Returns its id and name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The first prompt: everything the agent needs to know, since it does not see this conversation." },
                    "name": { "type": "string", "description": "A short name, a-z, 0-9 and -, at most 32; made from the task when left out." },
                    "worktree": { "type": "boolean", "description": "Run in a worktree of its own (default true); false runs it in this repository's checkout." },
                    "model": { "type": "string", "description": "The model, such as sonnet or opus; the default when left out." },
                    "base": { "type": "string", "description": "What its branch starts from (default: the repository's HEAD)." },
                    "outcome": { "type": "string", "description": "The id of the outcome (from add_outcome) it works on: it becomes that outcome's agent and is told so." }
                },
                "required": ["task"]
            }
        },
        {
            "name": "list_agents",
            "description": "The agents you started, with their state (starting, working, waiting, idle, exited), branch and last event; with all, every agent.",
            "inputSchema": { "type": "object", "properties": { "all": { "type": "boolean", "description": "Every agent, not only yours, the exited ones too." } } }
        },
        {
            "name": "read_agent",
            "description": "An agent's state and its events (prompts, tool calls, notifications, turns finished), one per line, after a sequence number.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" }, "after": { "type": "integer", "description": "Only events with a higher sequence number (the first number of each line)." } }, "required": ["id"] }
        },
        {
            "name": "wait_agent",
            "description": "Wait until an agent reaches a state: idle (its turn finished, the default), waiting (it asks the user something), exited, or any (any new event). Answers its state and its last events.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "until": { "type": "string", "enum": ["idle", "waiting", "exited", "any"] },
                    "timeout_s": { "type": "integer", "description": "At most this many seconds (default 300, at most 1800)." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "send_to_agent",
            "description": "Type text and Enter into an agent's terminal: a prompt when it is idle, an answer when it waits.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" }, "text": { "type": "string" } }, "required": ["id", "text"] }
        },
        {
            "name": "stop_agent",
            "description": "Stop an agent: its session ends (its worktree and branch are kept).",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] }
        },
        {
            "name": "set_preview",
            "description": "Say where the app you are working on runs (a dev server you started), so that the user sees it in Workbench's Preview and can point at what to change. Only http://127.0.0.1:<port> or http://localhost:<port>; an empty url clears it.",
            "inputSchema": { "type": "object", "properties": { "url": { "type": "string", "description": "Such as http://127.0.0.1:5173" } }, "required": ["url"] }
        },
        {
            "name": "ask_human",
            "description": "Ask the user a question in Workbench and wait for the answer (up to 30 minutes). Use it when only the user can decide: a choice of behaviour, scope or risk. Give 2 to 4 options and say which you recommend; the user can also answer in words, and may make the answer a rule that every agent of this repository is told from then on. Returns the answer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question, in one or two sentences the user can answer without reading your terminal." },
                    "options": { "type": "array", "description": "The choices.", "items": { "type": "object", "properties": { "label": { "type": "string", "description": "A few words." }, "detail": { "type": "string", "description": "What choosing it means." } }, "required": ["label"] } },
                    "recommended": { "type": "integer", "description": "The index of the option you recommend, from 0." },
                    "context": { "type": "string", "description": "What the user needs to know to decide: what you found, what each choice costs." }
                },
                "required": ["question"]
            }
        },
        {
            "name": "add_outcome",
            "description": "Add an outcome to your goal: a result the user can check, worked on by one agent. Returns its id, which spawn_agent takes as outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "What will be true when it is done, in under 10 words." },
                    "detail": { "type": "string", "description": "How to tell it is done." },
                    "after": { "type": "string", "description": "The id of an outcome it must wait for." }
                },
                "required": ["title"]
            }
        },
        {
            "name": "update_outcome",
            "description": "Update an outcome: its progress (0 to 100), its state (done when it checks out, dropped when it no longer makes sense), its title or detail. Without arguments but id, answers your goal's outcomes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "progress": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "state": { "type": "string", "enum": ["next", "active", "needs_you", "done", "dropped"] },
                    "title": { "type": "string" },
                    "detail": { "type": "string" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "list_incidents",
            "description": "Health's incidents in your repository: alert rules that fired on agents (a test failing again and again, tokens over a budget, two agents editing one file, an agent idle while asked), grouped by the agents and files they share, with what fires now.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "state": { "type": "string", "enum": ["open", "resolved", "all"], "description": "open (the default), resolved or all." }
                }
            }
        },
        {
            "name": "test_history",
            "description": "The test radar of your repository: each test's runs by every agent, pass rate, p50 and p99 duration, and which are flaky (passed and failed on the same code), newly failing or slow. With test, that test's runs, newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "test": { "type": "string", "description": "A test's name as the radar lists it, such as tests::totals::applies_coupon." },
                    "days": { "type": "integer", "minimum": 1, "maximum": 90, "description": "How far back, in days (default 7)." }
                }
            }
        },
        {
            "name": "blast_radius",
            "description": "What your change (or another agent's) reaches before it merges: the functions changed against the base, the tests that cover them, the decisions about those lines (from ChloeCtx), the other agents touching the same files or the callers, and a risk line. Run it before saying a change is done.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "description": "The agent; you when left out." } } }
        },
        {
            "name": "prepare_pr",
            "description": "Draft the pull request for your branch: packs a capsule of the decisions behind it (chloectx pack) and writes the body (summary, decisions with their ids, blast radius, tests run, how to ask the capsule) to a file. It does not push or open the pull request: it answers the body and the gh command for the user.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "string", "description": "The agent; you when left out." }, "capsule": { "type": "boolean", "description": "Pack the capsule first (default true)." } } }
        },
        {
            "name": "propose_change",
            "description": "Propose a command Workbench guards in this repository (a migration, a deploy script, a push to the default branch, a recursive rm outside your worktree, or one on its guarded list), which you may not run yourself: the user sees it in Workbench with your diff and approves or declines it. On approval Workbench runs it in your worktree and tells you its exit code and output; on decline, why. Carry on with what does not need it meanwhile.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command, as you would run it in your worktree." },
                    "why": { "type": "string", "description": "Why it is needed now, and what it changes, in a sentence or two the user can decide on." },
                    "risk": { "type": "string", "enum": ["low", "medium", "high"], "description": "How risky you think it is (Workbench judges too)." }
                },
                "required": ["command", "why"]
            }
        },
        {
            "name": "search_code",
            "description": "Search this repository's code by words or identifiers (cartTotal also finds cart_total), as your worktree has it: answers chunks (a function, a type or 60 lines) with path:start-end, best first. With tokens, answers one text of the best chunks that fits that many tokens, each under its // path:start-end line.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Words, identifiers or a phrase." },
                    "tokens": { "type": "integer", "minimum": 100, "maximum": 50000, "description": "A budget: answer the chunks' text packed into this many tokens." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": "How many chunks to list (default 10)." },
                    "lang": { "type": "string", "description": "Only this language: rust, typescript, javascript, python, go, markdown and so on." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "search_docs",
            "description": "Search the crawled docs of this repository's libraries (docs.rs, the libraries' own sites): answers pages with their URL, library and the passage that matches. Cite the URLs you rely on. With tokens, answers one text of the best passages that fits that many tokens.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "library": { "type": "string", "description": "Only this library's pages: its id, such as crates-serde or npm-react." },
                    "tokens": { "type": "integer", "minimum": 100, "maximum": 50000 }
                },
                "required": ["query"]
            }
        },
        // Preview lab (SPEC.md, "Fourth build", use cases 2 and 3).
        {
            "name": "test_with_persona",
            "description": "Have a simulated user try your app in your preview (set_preview first): a persona (first-timer, keyboard, careful, hurried, edge-cases, or a sentence describing someone) does a task in a real browser, driven by a model, and what got in their way comes back as findings. The user watches it in Workbench's Preview. It takes minutes and costs tokens: use it when a change is ready to be tried, not after every edit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "What the person tries to do, in their words, e.g. 'buy two mugs and pay with a card'." },
                    "persona": { "type": "string", "description": "A persona id (first-timer, keyboard, careful, hurried, edge-cases) or a sentence describing the person; first-timer when left out." },
                    "model": { "type": "string", "description": "The model playing the persona: haiku (the default), sonnet or opus." },
                    "wait": { "type": "boolean", "description": "Wait for the run to end and answer its findings (default true)." },
                    "timeout_s": { "type": "integer", "minimum": 10, "maximum": 1800, "description": "How long to wait, default 600." }
                },
                "required": ["task"]
            }
        },
        {
            "name": "runtime_errors",
            "description": "The open runtime errors of your app: what the previewed page threw or logged with console.error, the 5xx answers through the preview, and error logs your dev server sent over OTLP. Each comes with its stack and, when Workbench can tell, the edit that probably caused it (whose, which file and line, when).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "all": { "type": "boolean", "description": "Every agent's errors in this repository, not only your app's." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                }
            }
        },
        {
            "name": "trace",
            "description": "Ask, in a sentence, when, why or how something in the repository changed: \"why is retries 5?\", \"when and why did we change the rounding from floor to half-up?\". Answers from git on every branch (other agents' too), the edits Workbench saw, ChloeCtx's decisions and the user's answers and rules, with the code as it is now, each piece tagged [E1], [E2]. Ask it before changing something that someone decided, and cite what it found.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question; put a value, identifier or file in quotes or backticks to be exact." },
                    "repo": { "type": "string", "description": "The repository's absolute path; yours when left out." }
                },
                "required": ["question"]
            }
        },
        // Fifth build: Hotspots (src/hotspots.rs).
        crate::hotspots::mcp_def(),
        {
            "name": "check_intent",
            "description": "Check a task or goal before you hand it to an agent (spawn_agent, send_to_agent): says when it has no way to tell it is done, goes against a ChloeCtx decision or rule (with the record's id), names a guarded command or path, or overlaps a running agent's task or files. Cheap and deterministic: no model is asked.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The task, goal or message as you would send it." },
                    "field": { "type": "string", "enum": ["task", "goal", "steer", "answer"], "description": "What the text is; task when left out." },
                    "repo": { "type": "string", "description": "The repository's absolute path; yours when left out." }
                },
                "required": ["text"]
            }
        }
    ])
}

struct Client {
    env: Env,
}

impl Client {
    fn call(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value, String> {
        let url = format!("{}{path}", self.env.url);
        let agent = http_agent(timeout);
        let auth = format!("Bearer {}", self.env.token);
        // Who calls: the input it types is recorded as this agent's.
        let me = self.env.agent.as_str();
        let res = match body {
            Some(b) if method == "PUT" => agent.put(&url).header("authorization", &auth).send_json(b),
            Some(b) if method == "PATCH" => agent.patch(&url).header("authorization", &auth).header("x-workbench-agent", me).send_json(b),
            Some(b) if !me.is_empty() => agent.post(&url).header("authorization", &auth).header("x-workbench-agent", me).send_json(b),
            Some(b) => agent.post(&url).header("authorization", &auth).send_json(b),
            None if method == "POST" => agent.post(&url).header("authorization", &auth).send_empty(),
            None => agent.get(&url).header("authorization", &auth).call(),
        };
        let mut res = res.map_err(|e| format!("cannot reach Workbench at {}: {e}", self.env.url))?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "error": text }));
        if status >= 400 {
            return Err(v.get("error").and_then(Value::as_str).unwrap_or("error").to_string());
        }
        Ok(v)
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        self.call("GET", path, None, Duration::from_secs(15))
    }
}

/// `&repo=` with the caller's repository, or nothing (the server takes
/// the one most agents work in).
fn repo_param(c: &Client) -> String {
    if c.env.agent.is_empty() {
        return String::new();
    }
    match c.get(&format!("/api/agents/{}", enc(&c.env.agent))) {
        Ok(v) => v["agent"]["repo"].as_str().map(|r| format!("&repo={}", enc(r))).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn agent_line(a: &Value) -> String {
    let mut line = format!("{} {} [{}]", s(a, "id"), s(a, "name"), s(a, "state"));
    if !s(a, "branch").is_empty() {
        line.push_str(&format!(" branch {}", s(a, "branch")));
    }
    if s(a, "state") == "waiting" && !s(a, "waiting_for").is_empty() {
        line.push_str(&format!(" waiting for: {}", s(a, "waiting_for")));
    }
    if !s(a, "last_event").is_empty() {
        line.push_str(&format!(" | last: {}", s(a, "last_event")));
    }
    line
}

fn event_lines(evs: &Value) -> String {
    let lines: Vec<String> = evs
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| format!("{} {} {}: {}", e.get("seq").and_then(Value::as_i64).unwrap_or(0), s(e, "ts"), s(e, "kind"), s(e, "summary")))
        .collect();
    if lines.is_empty() {
        "(no events)".into()
    } else {
        lines.join("\n")
    }
}

fn id_arg(args: &Value) -> Result<String, String> {
    let id = s(args, "id");
    if id.is_empty() {
        return Err("id is required".into());
    }
    Ok(enc(id))
}

/// The agent's state and its last events, as text.
fn status(c: &Client, id: &str, last: usize) -> Result<String, String> {
    let a = c.get(&format!("/api/agents/{id}"))?;
    let evs = c.get(&format!("/api/agents/{id}/events?limit=1000"))?;
    let all = evs["events"].as_array().cloned().unwrap_or_default();
    let tail: Vec<Value> = all[all.len().saturating_sub(last)..].to_vec();
    Ok(format!("{}\nlast events:\n{}", agent_line(&a["agent"]), event_lines(&Value::Array(tail))))
}

fn tool(c: &Client, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "spawn_agent" => {
            let task = s(args, "task");
            if task.trim().is_empty() {
                return Err("task is required".into());
            }
            let repo = if c.env.agent.is_empty() {
                std::env::current_dir().map_err(|e| e.to_string())?.to_string_lossy().to_string()
            } else {
                let me = c.get(&format!("/api/agents/{}", enc(&c.env.agent)))?;
                s(&me["agent"], "repo").to_string()
            };
            let mut body = json!({ "repo": repo, "task": task, "worktree": args.get("worktree").and_then(Value::as_bool).unwrap_or(true) });
            for k in ["name", "model", "base", "outcome"] {
                if let Some(v) = args.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                    body[k] = json!(v);
                }
            }
            if !c.env.agent.is_empty() {
                body["parent"] = json!(c.env.agent);
            }
            // Without a model: the one tasks like this did best on, once 5
            // past tasks are similar (src/routing.rs); else the default.
            if body.get("model").is_none() {
                if let Ok(r) = c.get(&format!("/api/route?repo={}&task={}", enc(&repo), enc(task))) {
                    if r["apply"] == true {
                        if let Some(m) = r["suggestion"]["model"].as_str().filter(|m| *m != "default") {
                            body["model"] = json!(m);
                        }
                    }
                }
            }
            let v = c.call("POST", "/api/agents", Some(body), Duration::from_secs(60))?;
            let a = &v["agent"];
            Ok(format!(
                "Started agent {} (id {}) in {}{}. The user can watch it in Workbench. Use wait_agent with id {} to wait for its turn to finish.",
                s(a, "name"),
                s(a, "id"),
                s(a, "cwd"),
                if s(a, "branch").is_empty() { String::new() } else { format!(" on branch {}", s(a, "branch")) },
                s(a, "id")
            ))
        }
        "list_agents" => {
            let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
            let v = c.get("/api/agents?all=1")?;
            let lines: Vec<String> = v["agents"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|a| all || (!c.env.agent.is_empty() && s(a, "parent") == c.env.agent) || (c.env.agent.is_empty() && a.get("parent").is_none_or(Value::is_null)))
                .map(agent_line)
                .collect();
            Ok(if lines.is_empty() { "(no agents)".into() } else { lines.join("\n") })
        }
        "read_agent" => {
            let id = id_arg(args)?;
            let after = args.get("after").and_then(Value::as_i64).unwrap_or(0);
            let a = c.get(&format!("/api/agents/{id}"))?;
            let evs = c.get(&format!("/api/agents/{id}/events?after={after}&limit=1000"))?;
            Ok(format!("{}\n{}", agent_line(&a["agent"]), event_lines(&evs["events"])))
        }
        "wait_agent" => {
            let id = id_arg(args)?;
            let until = match s(args, "until") {
                "" => "idle",
                u @ ("idle" | "waiting" | "exited" | "any") => u,
                u => return Err(format!("until is one of idle, waiting, exited, any, not {u}")),
            };
            let total = args.get("timeout_s").and_then(Value::as_u64).unwrap_or(300).min(1800);
            let deadline = Instant::now() + Duration::from_secs(total);
            let reached = loop {
                let left = deadline.saturating_duration_since(Instant::now()).as_secs();
                let step = left.min(30);
                let v = c.call("GET", &format!("/api/wait?agent={id}&until={until}&timeout_s={step}"), None, Duration::from_secs(step + 15))?;
                let reached = v["reached"].as_bool().unwrap_or(false);
                let state = s(&v["agent"], "state");
                if reached || state == "exited" || Instant::now() >= deadline || step == 0 {
                    break reached;
                }
            };
            let head = if reached { format!("reached {until}") } else { format!("not {until} yet (waited up to {total} s)") };
            Ok(format!("{head}\n{}", status(c, &id, 10)?))
        }
        "send_to_agent" => {
            let id = id_arg(args)?;
            let text = s(args, "text");
            c.call("POST", &format!("/api/agents/{id}/input"), Some(json!({ "text": text, "enter": true })), Duration::from_secs(15))?;
            Ok(format!("typed into {} and pressed Enter", s(args, "id")))
        }
        "stop_agent" => {
            let id = id_arg(args)?;
            c.call("POST", &format!("/api/agents/{id}/stop"), None, Duration::from_secs(15))?;
            Ok(format!("stopping {} (SIGHUP, then SIGKILL after 5 s)", s(args, "id")))
        }
        "set_preview" => {
            if c.env.agent.is_empty() {
                return Err("set_preview is for an agent of Workbench: CHLOEDB_WORKBENCH_AGENT is not set".into());
            }
            let url = s(args, "url").trim();
            let v = c.call("PUT", &format!("/api/agents/{}/preview", enc(&c.env.agent)), Some(json!({ "url": url })), Duration::from_secs(15))?;
            Ok(match v["agent"]["preview_url"].as_str() {
                Some(u) => format!("The preview is {u}: the user sees it in Workbench's Preview, and can pick an element there to send you."),
                None => "The preview is cleared.".into(),
            })
        }
        "ask_human" => ask_human(c, args),
        "add_outcome" => {
            let title = s(args, "title");
            if title.trim().is_empty() {
                return Err("title is required".into());
            }
            let mut body = json!({ "title": title });
            for k in ["detail", "after"] {
                if let Some(v) = args.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                    body[k] = json!(v);
                }
            }
            let v = c.call("POST", "/api/outcomes", Some(body), Duration::from_secs(15))?;
            let o = &v["outcome"];
            Ok(format!("Added outcome {} ({}) to goal {}. Start its agent with spawn_agent(outcome=\"{}\", task=...).", s(o, "id"), s(o, "title"), s(o, "goal"), s(o, "id")))
        }
        "update_outcome" => {
            let id = id_arg(args)?;
            let mut body = json!({});
            for k in ["state", "title", "detail"] {
                if let Some(v) = args.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
                    body[k] = json!(v);
                }
            }
            if let Some(p) = args.get("progress").and_then(Value::as_i64) {
                body["progress"] = json!(p);
            }
            let v = c.call("PATCH", &format!("/api/outcomes/{id}"), Some(body), Duration::from_secs(15))?;
            let o = &v["outcome"];
            let all = c.get(&format!("/api/outcomes?goal={}", enc(s(o, "goal"))))?;
            let lines: Vec<String> = all["outcomes"].as_array().into_iter().flatten().map(outcome_line).collect();
            Ok(format!("{}\ngoal {}'s outcomes:\n{}", outcome_line(o), s(o, "goal"), lines.join("\n")))
        }
        "list_incidents" => {
            let state = match s(args, "state") {
                "" => "open",
                x => x,
            };
            let v = c.get(&format!("/api/incidents?state={}{}", enc(state), repo_param(c)))?;
            Ok(crate::alerts::incidents_text(&v))
        }
        "test_history" => {
            let test = s(args, "test").trim();
            if test.is_empty() {
                let days = args.get("days").and_then(Value::as_i64).unwrap_or(7);
                let v = c.get(&format!("/api/tests?days={days}{}", repo_param(c)))?;
                Ok(crate::testruns::radar_text(&v))
            } else {
                let v = c.get(&format!("/api/tests/history?test={}{}", enc(test), repo_param(c)))?;
                Ok(crate::testruns::history_text(&v))
            }
        }
        "blast_radius" => {
            let id = me_or(c, args)?;
            let b = c.call("GET", &format!("/api/agents/{id}/blast"), None, Duration::from_secs(300))?;
            Ok(blast_text(&b))
        }
        "prepare_pr" => {
            let id = me_or(c, args)?;
            let mut note = String::new();
            if args.get("capsule").and_then(Value::as_bool).unwrap_or(true) {
                let cap = c.call("POST", &format!("/api/agents/{id}/capsule"), Some(json!({})), Duration::from_secs(660))?;
                note = match cap["capsule"]["dir"].as_str() {
                    Some(d) => format!("Packed the capsule in {d} ({} records).\n", cap["capsule"]["records"]),
                    None => format!("No capsule: {}\n", s(&cap, "why")),
                };
            }
            let p = c.call("GET", &format!("/api/agents/{id}/pr"), None, Duration::from_secs(300))?;
            Ok(format!(
                "{note}Title: {}\nThe body is in {}. Push the branch and open it (the user decides): {}\n  {}\n\n{}",
                s(&p, "title"),
                s(&p, "body_file"),
                s(&p, "push"),
                s(&p, "gh"),
                s(&p, "body")
            ))
        }
        "propose_change" => {
            if c.env.agent.is_empty() {
                return Err("propose_change is for an agent of Workbench: CHLOEDB_WORKBENCH_AGENT is not set".into());
            }
            let command = s(args, "command");
            if command.trim().is_empty() {
                return Err("command is required".into());
            }
            let mut body = json!({ "command": command, "why": s(args, "why") });
            if !s(args, "risk").is_empty() {
                body["risk"] = json!(s(args, "risk"));
            }
            let v = c.call("POST", "/api/changes", Some(body), Duration::from_secs(30))?;
            let r = &v["request"];
            Ok(format!(
                "Proposed as change request {} ({} risk{}). The user sees it in Workbench and approves or declines it; you will be told the result here, with the command's output. Carry on with what does not depend on it.",
                s(r, "id"),
                s(r, "risk"),
                if s(r, "rule_label").is_empty() { String::new() } else { format!(", guarded by: {}", s(r, "rule_label")) }
            ))
        }
        "search_code" | "search_docs" => search_tool(c, name, args),
        "test_with_persona" => crate::swarm::mcp_tool(&|m, p, b, t| c.call(m, p, b, Duration::from_secs(t)), &c.env.agent, args),
        "runtime_errors" => crate::runtime::mcp_tool(&|m, p, b, t| c.call(m, p, b, Duration::from_secs(t)), &c.env.agent, args),
        "trace" => {
            let question = s(args, "question").trim();
            if question.is_empty() {
                return Err("question is required".into());
            }
            let place = match args.get("repo").and_then(Value::as_str).filter(|r| !r.is_empty()) {
                Some(r) => format!("repo={}", enc(r)),
                None if !c.env.agent.is_empty() => format!("agent={}", enc(&c.env.agent)),
                None => return Err("repo is required outside a Workbench session".into()),
            };
            let v = c.call("GET", &format!("/api/trace?q={}&{place}", enc(question)), None, Duration::from_secs(240))?;
            Ok(crate::trace::as_text(&v))
        }
        "hotspots" => crate::hotspots::mcp_tool(&|m, p, b, t| c.call(m, p, b, Duration::from_secs(t)), &c.env.agent, args),
        "check_intent" => {
            let text = s(args, "text");
            if text.trim().is_empty() {
                return Err("text is required".into());
            }
            let repo = args.get("repo").and_then(Value::as_str).filter(|r| !r.is_empty());
            if repo.is_none() && c.env.agent.is_empty() {
                return Err("repo is required outside a Workbench session".into());
            }
            let body = json!({ "text": text, "field": args.get("field").and_then(Value::as_str).unwrap_or("task"), "repo": repo, "agent": c.env.agent });
            let v = c.call("POST", "/api/intent/check", Some(body), Duration::from_secs(30))?;
            Ok(crate::complete::as_text(&v))
        }
        other => Err(format!("no tool {other}")),
    }
}

/// The agent a Ship tool is about: `id`, else the caller.
fn me_or(c: &Client, args: &Value) -> Result<String, String> {
    match s(args, "id") {
        "" if c.env.agent.is_empty() => Err("id is required outside an agent of Workbench".into()),
        "" => Ok(enc(&c.env.agent)),
        id => Ok(enc(id)),
    }
}

/// A blast radius as lines.
fn blast_text(b: &Value) -> String {
    let mut out = vec![s(&b["risk"], "line").to_string()];
    let list = |k: &str| b[k].as_array().cloned().unwrap_or_default();
    out.push("Functions changed:".into());
    for f in list("functions") {
        let tests = f["tests"].as_array().map(|t| t.len()).unwrap_or(0);
        out.push(format!(
            "- {} ({}:{}, {}) {} callers, {}",
            s(&f, "name"),
            s(&f, "path"),
            f["line"],
            s(&f, "status"),
            f["callers_total"],
            if tests == 0 { "NO TEST names it".to_string() } else { format!("{tests} test files") }
        ));
    }
    out.push("Tests that cover them:".into());
    for t in list("tests") {
        let r = &t["runs"];
        let runs = if r.is_null() { "not run".to_string() } else { format!("{} of {} passed{}", r["passed"], r["total"], if r["flaky"] == true { ", flaky" } else { "" }) };
        out.push(format!("- {} ({runs}): {}", s(&t, "path"), t["why"].as_array().map(|w| w.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default()));
    }
    out.push("Decisions about these lines (ChloeCtx):".into());
    for d in list("decisions") {
        out.push(format!("- [{}] {}{}: {}", s(&d, "id"), s(&d, "kind"), if d["superseded"] == true { " (superseded)" } else { "" }, s(&d, "text")));
    }
    out.push("Other agents in the way:".into());
    for g in list("agents") {
        out.push(format!("- {}", s(&g, "detail")));
    }
    for m in list("missing") {
        out.push(format!("(missing: {})", m.as_str().unwrap_or("")));
    }
    out.join("\n")
}

fn outcome_line(o: &Value) -> String {
    let mut line = format!("{} [{}] {}", s(o, "id"), s(o, "state"), s(o, "title"));
    if let Some(p) = o["progress"].as_i64().filter(|p| *p > 0 && *p < 100) {
        line.push_str(&format!(" {p}%"));
    }
    if !s(o, "agent_name").is_empty() {
        line.push_str(&format!(" (agent {})", s(o, "agent_name")));
    }
    if !s(o, "after").is_empty() {
        line.push_str(&format!(" after {}", s(o, "after")));
    }
    line
}

/// What an answered question says to the agent that asked it.
fn answer_text(q: &Value) -> String {
    let by = s(q, "answered_by");
    let who = if by.starts_with("rule:") { format!("A rule the user set ({}) answers it", by.trim_start_matches("rule:")) } else { "The user answered".to_string() };
    let mut out = format!("{who}: {}", s(q, "answer"));
    if !s(q, "rule").is_empty() && !by.starts_with("rule:") {
        out.push_str("\n(The user made this a rule for the repository: follow it from now on.)");
    }
    out
}

/// `ask_human`: posts the question, then waits for its answer in 30 s steps.
fn ask_human(c: &Client, args: &Value) -> Result<String, String> {
    if c.env.agent.is_empty() {
        return Err("ask_human needs CHLOEDB_WORKBENCH_AGENT: it runs inside a Workbench session".into());
    }
    let question = s(args, "question");
    if question.trim().is_empty() {
        return Err("question is required".into());
    }
    let mut body = json!({ "question": question, "options": args.get("options").cloned().unwrap_or(json!([])) });
    for k in ["recommended", "context"] {
        if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
            body[k] = v.clone();
        }
    }
    let v = c.call("POST", "/api/questions", Some(body), Duration::from_secs(30))?;
    let q = v["question"].clone();
    let id = s(&q, "id").to_string();
    if s(&q, "state") == "answered" {
        return Ok(answer_text(&q));
    }
    let deadline = Instant::now() + Duration::from_secs(ASK_WAIT_S);
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).as_secs();
        if left == 0 {
            return Ok(format!("No answer yet after {} minutes (question {id}). Carry on with what does not depend on it, or ask again later.", ASK_WAIT_S / 60));
        }
        let step = left.min(30);
        let v = c.call("GET", &format!("/api/questions/{}/wait?timeout_s={step}", enc(&id)), None, Duration::from_secs(step + 15))?;
        let q = &v["question"];
        match s(q, "state") {
            "answered" => return Ok(answer_text(q)),
            "expired" => return Ok(format!("The question {id} expired without an answer.")),
            _ => {}
        }
    }
}

fn handle(c: Option<&Client>, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = s(msg, "method");
    let id = id?; // a notification: no answer
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "workbench", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            let name = s(&params, "name").to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let out = match c {
                Some(c) => tool(c, &name, &args),
                None => Err("CHLOEDB_WORKBENCH_URL and CHLOEDB_WORKBENCH_TOKEN are not set: this server runs inside a Workbench session".into()),
            };
            Ok(match out {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
            })
        }
        "resources/list" => Ok(json!({ "resources": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        _ => Err(json!({ "code": -32601, "message": format!("method not found: {method}") })),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    })
}

pub fn run() {
    let client = Env::from_env().map(|env| Client { env });
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(client.as_ref(), &msg),
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } })),
        };
        if let Some(a) = answer {
            if writeln!(out, "{a}").is_err() || out.flush().is_err() {
                break;
            }
        }
    }
}

/// `search_code` and `search_docs`: the caller's search, logged as its own.
fn search_tool(c: &Client, name: &str, args: &Value) -> Result<String, String> {
    if c.env.agent.is_empty() {
        return Err(format!("{name} is for an agent of Workbench: CHLOEDB_WORKBENCH_AGENT is not set"));
    }
    let query = s(args, "query").trim();
    if query.is_empty() {
        return Err("query is required".into());
    }
    let me = enc(&c.env.agent);
    let tokens = args.get("tokens").and_then(Value::as_i64).filter(|t| *t > 0);
    let mut path = if name == "search_code" {
        let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(10).clamp(1, 50);
        let mut p = format!("/api/search/code?q={}&agent={me}&by={me}&limit={limit}", enc(query));
        if !s(args, "lang").is_empty() {
            p.push_str(&format!("&lang={}", enc(s(args, "lang"))));
        }
        p
    } else {
        let mut p = format!("/api/search/docs?q={}&by={me}&limit=8", enc(query));
        if !s(args, "library").is_empty() {
            p.push_str(&format!("&library={}", enc(s(args, "library"))));
        }
        p
    };
    if let Some(t) = tokens {
        path.push_str(&format!("&tokens={t}"));
    }
    let v = c.call("GET", &path, None, Duration::from_secs(40))?;
    let mut out = Vec::new();
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    if let Some(p) = v.get("pack").filter(|p| p.is_object()) {
        let taken = p["rows"].as_array().into_iter().flatten().filter(|r| !s(r, "decision").starts_with("skip")).count();
        out.push(format!("PACK {} TOKENS: {} used, {taken} {} taken.", p["budget"], p["used"], if name == "search_code" { "chunks" } else { "pages" }));
        out.push(s(p, "text").to_string());
    } else if name == "search_code" {
        out.push(format!("{} chunks for {query:?}{}:", hits.len(), if v["agent"]["cwd"].as_str().is_some_and(|w| w != s(&v["agent"], "repo")) { " as your worktree has them" } else { "" }));
        for h in &hits {
            let sym = [s(h, "kind"), s(h, "symbol")].iter().filter(|x| !x.is_empty() && **x != "lines" && **x != "head").copied().collect::<Vec<_>>().join(" ");
            out.push(format!("{}:{}-{} {sym}", s(h, "path"), h["start_line"], h["end_line"]).trim_end().to_string());
            for l in s(h, "text").lines().filter(|l| !l.trim().is_empty()).take(3) {
                out.push(format!("    {}", l.chars().take(160).collect::<String>()));
            }
        }
    } else {
        out.push(format!("{} pages for {query:?}:", hits.len()));
        for h in &hits {
            out.push(format!("{} <{}> ({})", s(h, "title"), s(h, "url"), s(&h["library"], "name")));
            out.push(format!("    {}", s(h, "text")));
        }
        if !hits.is_empty() {
            out.push("Cite the URLs you rely on.".into());
        }
    }
    for m in v["missing"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        out.push(format!("note: {m}"));
    }
    Ok(out.join("\n"))
}
