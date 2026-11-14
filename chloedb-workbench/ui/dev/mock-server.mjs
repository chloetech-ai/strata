// A stand-in for `chloedb-workbench serve`, for working on the UI without the
// Rust backend: it serves ../ (the UI's files) and fakes SPEC.md's API with
// a few agents that change state over time and print a Claude Code-like
// screen. Node 22, standard library only (the WebSocket is written by hand).
//
//   node chloedb-workbench/ui/dev/mock-server.mjs [--port 7741] [--token dev] [--agents 4] [--fast] [--no-story]
//
// Then open the URL it prints. Keystrokes typed into a zoomed terminal are
// echoed; Enter answers a waiting agent or gives an idle one a new turn.
//
// The decision trail (SPEC.md, second build) is faked by ./trail-story.mjs:
// a lead agent in /home/user/src/invoicer that spawned two workers, their
// diffs, the decisions ChloeCtx distilled, a recording of each session, and
// the routes /diff, /recording, /api/why, /api/decisions and /api/ask.

import http from "node:http";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { buildStory } from "./trail-story.mjs";

const args = process.argv.slice(2);
const opt = (name, def) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : def;
};
const PORT = Number(opt("port", 7741));
const TOKEN = opt("token", "dev");
const START_AGENTS = Number(opt("agents", 4));
const STORY = !args.includes("--no-story");
const FAST = args.includes("--fast");
const MAX_AGENTS = 8;
const UI_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const SCROLLBACK = 256 * 1024;

// --------------------------------------------------------------- the model

const agents = new Map(); // id -> { row, buf, socks:Set, timer, script, step, line }
let seq = 0;
const events = []; // all events, global order
const streams = new Set(); // SSE responses

const now = () => Date.now();
const iso = (ms) => new Date(ms).toISOString();
const hex = (n) => crypto.randomBytes(n).toString("hex");

function agentJson(a) {
  const r = a.row;
  return {
    ...r,
    state_since: iso(r.state_since),
    created: iso(r.created),
    ended: r.ended ? iso(r.ended) : null,
    running: r.state !== "exited",
    children: [...agents.values()].filter((o) => o.row.parent === r.id).map((o) => o.row.id),
  };
}

function broadcast(kind, data) {
  const msg = `event: ${kind}\ndata: ${JSON.stringify(data)}\n\n`;
  for (const res of streams) res.write(msg);
}

function addEvent(a, kind, tool, summary, at = now()) {
  seq++;
  const ev = { id: `${a.row.id}:${String(seq).padStart(10, "0")}`, seq, ts: iso(at), agent: a.row.id, kind, tool: tool || null, summary: summary || "" };
  events.push(ev);
  a.row.last_event = summary || kind;
  broadcast("event", ev);
  return ev;
}

function setState(a, state, extra = {}, at = now()) {
  const r = a.row;
  if (r.state !== state) { r.state = state; r.state_since = at; }
  if (state !== "waiting") r.waiting_for = null;
  Object.assign(r, extra);
  broadcast("agent", agentJson(a));
}

function out(a, text) {
  const b = Buffer.from(text.replace(/\n/g, "\r\n"));
  a.buf = Buffer.concat([a.buf, b]);
  if (a.buf.length > SCROLLBACK) a.buf = a.buf.subarray(a.buf.length - SCROLLBACK);
  for (const s of a.socks) s.send(b, 2);
}

// ANSI helpers: Claude Code's look, roughly.
const E = "\x1b[";
const orange = (s) => `${E}38;2;215;119;87m${s}${E}0m`;
const dim = (s) => `${E}2m${s}${E}22m`;
const gray = (s) => `${E}90m${s}${E}0m`;
const bold = (s) => `${E}1m${s}${E}22m`;
const green = (s) => `${E}32m${s}${E}0m`;
const red = (s) => `${E}31m${s}${E}0m`;
const blue = (s) => `${E}38;2;87;150;230m${s}${E}0m`;

function box(lines, w = 74, color = gray) {
  const pad = (s) => {
    const visible = s.replace(/\x1b\[[0-9;]*m/g, "");
    return s + " ".repeat(Math.max(0, w - 4 - [...visible].length));
  };
  return [color(`╭${"─".repeat(w - 2)}╮`), ...lines.map((l) => `${color("│")} ${pad(l)} ${color("│")}`), color(`╰${"─".repeat(w - 2)}╯`)].join("\n") + "\n";
}

const TOOLS = [
  ["Read", (r) => `${r.files[0]}`, (r) => `Read ${100 + (r.n * 37) % 400} lines`],
  ["Grep", (r) => `pattern: "${r.grep}"`, (r) => `Found ${2 + r.n % 9} files`],
  ["Edit", (r) => `${r.files[1]}`, () => `Updated with 6 additions and 2 removals`],
  ["Bash", (r) => r.cmd, (r) => (r.n % 3 ? "test result: ok. 41 passed; 0 failed" : "warning: unused variable `span`")],
  ["Read", (r) => `${r.files[2]}`, () => `Read 88 lines`],
  ["Bash", () => "git diff --stat", () => "3 files changed, 48 insertions(+), 11 deletions(-)"],
];

const SAYS = [
  "I'll start by finding where this is handled.",
  "That's the function. Let me look at its callers before changing it.",
  "The bug is an off-by-one when the input ends inside a token.",
  "Now let me run the tests to see what fails.",
  "Two tests depended on the old behaviour; updating them.",
  "Everything passes. Let me check the diff once more.",
];

const ASKS = [
  ["Claude needs your permission to use Bash", "cargo test --release -p chloedb -- tokenizer"],
  ["Claude needs your permission to use Edit", "chloedb/src/sql/lexer.rs"],
  ["Claude is waiting for your input", null],
];

const PRESETS = [
  { name: "fix-lexer", task: "Fix the SQL lexer so a string literal that ends at the end of the input no longer drops its last character; add a test.", model: "opus", files: ["chloedb/src/sql/lexer.rs", "chloedb/src/sql/lexer.rs", "chloedb/tests/sql_lexer.rs"], grep: "fn next_token", cmd: "cargo test -p chloedb lexer", start: "waiting", ask: 0, ago: 252 },
  { name: "docs-guide", task: "Read docs/guide/*.md and fix every code block whose output no longer matches what docs/guide/run.py prints.", model: "sonnet", files: ["docs/guide/search.md", "docs/guide/search.md", "docs/guide/run.py"], grep: "```console", cmd: "python3 docs/guide/run.py --list", start: "working", ago: 41 },
  { name: "bench-postings", task: "Benchmark the posting-list codecs in chloedb/src/postings on the SciFact collection and write the results table.", model: "opus", files: ["chloedb/src/postings/mod.rs", "chloedb/benches/postings.rs", "chloedb/src/postings/pfor.rs"], grep: "impl Codec", cmd: "cargo bench --bench postings -- --quick", start: "idle", ago: 610 },
  { name: "ui-a11y", task: "Audit the command center's dialogs for keyboard focus traps and missing labels.", model: null, files: ["chloedb-server/ui/app/core.js", "chloedb-server/ui/app/main.js", "chloedb-server/ui/index.html"], grep: "showModal", cmd: "node --check chloedb-server/ui/app/core.js", start: "working", ago: 8 },
  { name: "flaky-replica", task: "Find why tests/replication.rs::follower_catches_up fails one run in fifty on Linux.", model: "opus", files: ["chloedb-server/tests/replication.rs", "chloedb-server/src/replica.rs", "chloedb-server/src/wal.rs"], grep: "catch_up", cmd: "cargo test --test all replication:: -- --nocapture", start: "starting", ago: 2 },
  { name: "sheets-xlsx", task: "Import merged cells from xlsx files in chloedb-sheets.", model: "sonnet", files: ["chloedb-sheets/src/xlsx.rs", "chloedb-sheets/src/xlsx.rs", "chloedb-sheets/tests/xlsx.rs"], grep: "merge", cmd: "cargo test xlsx", start: "working", ago: 90 },
];

function header(a) {
  const r = a.row;
  out(a, "\x1b[2J\x1b[H");
  out(a, box([`${orange("✻")} ${bold("Welcome to Claude Code!")}`, "", gray(`  /help for help, /status for your current setup`), "", gray(`  cwd: ${r.cwd.length > 80 ? "…" + r.cwd.slice(-79) : r.cwd}`)], 92, orange));
  out(a, "\n");
  if (r.task) out(a, `${gray(">")} ${r.task}\n\n`);
}

function prompt(a, hint) {
  out(a, box([`${gray(">")} ${hint ? gray(hint) : ""}`], 74));
  out(a, gray(`  ? for shortcuts${" ".repeat(40)}${r(a)}\n`));
  function r(x) { return x.row.model ? `model: ${x.row.model}` : ""; }
}

function delay(ms) { return FAST ? Math.max(200, ms / 4) : ms; }

function schedule(a, ms, fn) {
  clearTimeout(a.timer);
  a.timer = setTimeout(() => { if (a.row.state !== "exited") fn(); }, delay(ms));
}

function work(a) {
  const r = a.row;
  const step = a.step++;
  if (step === 0) {
    setState(a, "working");
    addEvent(a, "UserPromptSubmit", null, `prompt: ${(r.task || "").slice(0, 200)}`);
  }
  const say = SAYS[step % SAYS.length];
  const [tool, arg, res] = TOOLS[step % TOOLS.length];
  out(a, `${orange("●")} ${say}\n\n`);
  out(a, `${orange("●")} ${bold(tool)}(${arg(a.preset)})\n`);
  addEvent(a, "PreToolUse", tool, `${tool}: ${arg(a.preset)}`);
  out(a, `${orange("✻")} ${orange("Pondering…")} ${gray("(esc to interrupt)")}`);
  schedule(a, 1800 + Math.random() * 2400, () => {
    out(a, "\r\x1b[2K");
    const result = res(a.preset);
    out(a, `  ${gray("⎿")}  ${result.includes("warning") ? red(result) : result.includes("ok") ? green(result) : gray(result)}\n\n`);
    addEvent(a, "PostToolUse", tool, `${tool} done: ${result}`);
    a.preset.n++;
    const turn = step + 1;
    const roll = Math.random();
    if (turn > 4 && roll < 0.12) return ask(a, ASKS[step % 2]);
    if (turn > 6 && roll < 0.2) return finish(a);
    schedule(a, 900 + Math.random() * 1500, () => work(a));
  });
}

function ask(a, [msg, detail]) {
  if (detail && msg.includes("Bash")) {
    out(a, box([bold("Bash command"), "", `  ${detail}`, gray("  Run the tokenizer's tests"), "", "Do you want to proceed?", `${blue("❯")} ${blue("1. Yes")}`, "  2. Yes, and don't ask again for cargo test commands", `  3. No, and tell Claude what to do differently ${gray("(esc)")}`], 74, blue));
  } else if (detail) {
    out(a, box([bold("Edit file"), "", `  ${detail}`, "", "Do you want to make this edit?", `${blue("❯")} ${blue("1. Yes")}`, "  2. Yes, allow all edits during this session", `  3. No, and tell Claude what to do differently ${gray("(esc)")}`], 74, blue));
  } else {
    prompt(a);
  }
  a.asking = true;
  addEvent(a, "Notification", null, msg);
  setState(a, "waiting", { waiting_for: msg });
}

function finish(a) {
  const r = a.row;
  out(a, `${orange("●")} Done. ${r.task ? "The change is in place and the tests pass." : "Anything else?"}\n\n`);
  addEvent(a, "Stop", null, "turn finished");
  prompt(a);
  a.asking = false;
  setState(a, "idle");
}

function exitAgent(a, code = 0) {
  clearTimeout(a.timer);
  out(a, `\n${gray(`[process exited with code ${code}]`)}\n`);
  addEvent(a, "exited", null, `Exited with code ${code}`);
  setState(a, "exited", { exit_code: code, ended: now(), pid: a.row.pid });
  for (const s of a.socks) s.close();
}

function onInput(a, text) {
  if (a.row.state === "exited") return;
  for (const ch of text) {
    if (ch === "\r") {
      const line = a.line;
      a.line = "";
      out(a, "\n");
      if (a.row.state === "waiting" || a.row.state === "idle") {
        addEvent(a, "input", null, line ? `typed: ${line.slice(0, 80)}` : "pressed Enter");
        if (a.row.state === "idle" && line) { out(a, `${gray(">")} ${line}\n\n`); addEvent(a, "UserPromptSubmit", null, `prompt: ${line.slice(0, 200)}`); }
        a.asking = false;
        setState(a, "working");
        schedule(a, 600, () => work(a));
      }
    } else if (ch === "\x7f") {
      if (a.line) { a.line = a.line.slice(0, -1); out(a, "\b \b"); }
    } else if (ch === "\x1b") {
      if (a.row.state === "working") { clearTimeout(a.timer); out(a, `\r\x1b[2K  ${red("⎿  Interrupted by user")}\n`); finish(a); }
    } else if (ch === "\x03") {
      out(a, gray("^C"));
    } else if (ch >= " ") {
      a.line += ch;
      out(a, ch);
    }
  }
}

function spawn({ repo, task, name, branch, model, worktree = true, parent = null, preset }, opts = {}) {
  const id = `a${hex(4)}`;
  const p = preset || { files: ["src/main.rs", "src/lib.rs", "tests/basic.rs"], grep: "TODO", cmd: "cargo test", n: 0 };
  p.n = p.n || 0;
  const nm = name || preset?.name || `agent-${hex(2)}`;
  const repoName = path.basename(repo);
  const t = now() - (opts.ago || 0) * 1000;
  const a = {
    row: {
      id, name: nm, repo, cwd: worktree ? `/home/user/.local/share/chloedb-workbench/worktrees/${repoName}-${hex(4)}/${nm}` : repo,
      branch: branch || (worktree ? `wb/${nm}` : "main"), task: task || null, parent, depth: parent ? (agents.get(parent)?.row.depth ?? 0) + 1 : 0,
      state: "starting", state_since: t, waiting_for: null, last_event: null, session_id: crypto.randomUUID(), pid: 40000 + Math.floor(Math.random() * 20000),
      exit_code: null, model: model || null, created: t - 60_000 * Math.random(), ended: null, base: hex(20),
    },
    buf: Buffer.alloc(0), socks: new Set(), timer: null, step: 0, line: "", preset: p, asking: false,
  };
  a.row.started_ms = Math.round(a.row.created);
  agents.set(id, a);
  addEvent(a, "spawned", null, `Spawned${parent ? ` by ${agents.get(parent)?.row.name}` : ""} in ${a.row.cwd}`, t);
  broadcast("agent", agentJson(a));
  header(a);
  return a;
}

function boot(a, start) {
  if (start === "starting") { schedule(a, 3000, () => { addEvent(a, "SessionStart", null, "session started (startup)"); a.row.task ? work(a) : (prompt(a), setState(a, "idle")); }); return; }
  addEvent(a, "SessionStart", null, "session started (startup)", a.row.state_since);
  if (!a.row.task) { prompt(a); setState(a, "idle", {}, a.row.state_since); return; }
  // Play a few steps into the scrollback at once, then settle in the wanted state.
  const at = a.row.state_since;
  out(a, `${orange("●")} ${SAYS[0]}\n\n`);
  for (let i = 0; i < 3; i++) {
    const [tool, arg, res] = TOOLS[i];
    out(a, `${orange("●")} ${bold(tool)}(${arg(a.preset)})\n  ${gray("⎿")}  ${gray(res(a.preset))}\n\n`);
    addEvent(a, "PreToolUse", tool, `${tool}: ${arg(a.preset)}`, at - (4 - i) * 9000);
    addEvent(a, "PostToolUse", tool, `${tool} done: ${res(a.preset)}`, at - (4 - i) * 9000 + 2500);
    a.preset.n++;
  }
  a.step = 3;
  if (start === "waiting") { const [msg, detail] = ASKS[a.preset.ask || 0]; a.row.state = "working"; ask(a, [msg, detail]); a.row.state_since = at; broadcast("agent", agentJson(a)); return; }
  if (start === "idle") { finish(a); a.row.state_since = at; broadcast("agent", agentJson(a)); return; }
  setState(a, "working", {}, at);
  work(a);
}

for (const preset of PRESETS.slice(0, START_AGENTS)) {
  const a = spawn({ repo: "/home/user/chloedb", task: preset.task, model: preset.model, preset: { ...preset, n: 0 } }, { ago: preset.ago + 30 });
  a.row.state_since = now() - preset.ago * 1000;
  boot(a, preset.start);
}
// Two that ended earlier, for the fold.
for (const [nm, code, task] of [["try-simd", 0, "Try a SIMD intersect for two posting lists and report whether it is faster."], ["refactor-wal", 1, "Split wal.rs into a reader and a writer module."]]) {
  const a = spawn({ repo: "/home/user/chloedb", task, name: nm }, { ago: 3600 + code * 1800 });
  out(a, `${orange("●")} Finished.\n`);
  exitAgent(a, code);
  a.row.ended = now() - (1200 + code * 900) * 1000;
  a.row.state_since = a.row.ended;
}
// One spawned by another agent, to show parents and children.
if (START_AGENTS >= 3) {
  const parent = [...agents.values()].find((x) => x.row.name === "bench-postings");
  if (parent) {
    const c = spawn({ repo: "/home/user/chloedb", task: "Run the SciFact relevance gate on the pfor codec branch and report nDCG@10.", name: "bench-gate", parent: parent.row.id, model: "haiku", preset: { files: ["chloedb/tests/relevance_gate.rs", "chloedb/tests/relevance_gate.rs", "chloedb/tests/fixtures/beir/fetch.sh"], grep: "ndcg", cmd: "cargo test --release --test relevance_gate", n: 0 } }, { ago: 300 });
    c.row.state_since = now() - 120_000;
    boot(c, "working");
  }
}

// A row made whole by the caller (the story), with no events or screen yet.
function makeAgent(fields, preset) {
  const id = `a${hex(4)}`;
  const row = {
    id, name: null, repo: null, cwd: null, branch: null, task: null, parent: null, depth: 0, state: "starting", state_since: now(),
    waiting_for: null, last_event: null, session_id: crypto.randomUUID(), pid: null, exit_code: null, model: null, created: now(), ended: null,
    base: null, started_ms: null, ...fields,
  };
  const a = { row, buf: Buffer.alloc(0), socks: new Set(), timer: null, step: 0, line: "", preset: preset || { files: ["src/main.rs"], grep: "TODO", cmd: "make", n: 0 }, asking: false };
  agents.set(id, a);
  return a;
}

// The decision trail's story (./trail-story.mjs), and its routes.
const trail = buildStory({ agents, makeAgent, addEvent, agentJson, now, events });
if (!STORY) for (const a of Object.values(trail.story)) agents.delete(a.row.id);

// ------------------------------------------------------------- websocket

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

function wsAccept(req, socket, onMessage, onClose) {
  const key = req.headers["sec-websocket-key"];
  const accept = crypto.createHash("sha1").update(key + WS_GUID).digest("base64");
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  let buf = Buffer.alloc(0);
  let closed = false;
  const ws = {
    send(data, opcode = 2) {
      if (closed) return;
      const payload = Buffer.isBuffer(data) ? data : Buffer.from(data);
      const n = payload.length;
      const head = n < 126 ? Buffer.from([0x80 | opcode, n]) : n < 65536 ? Buffer.from([0x80 | opcode, 126, n >> 8, n & 255]) : (() => { const b = Buffer.alloc(10); b[0] = 0x80 | opcode; b[1] = 127; b.writeBigUInt64BE(BigInt(n), 2); return b; })();
      socket.write(Buffer.concat([head, payload]));
    },
    close() { if (!closed) { try { socket.write(Buffer.from([0x88, 0])); } catch { /* gone */ } closed = true; socket.end(); } },
  };
  socket.on("data", (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const op = buf[0] & 15;
      let len = buf[1] & 127, off = 2;
      if (len === 126) { if (buf.length < 4) return; len = buf.readUInt16BE(2); off = 4; }
      else if (len === 127) { if (buf.length < 10) return; len = Number(buf.readBigUInt64BE(2)); off = 10; }
      const masked = buf[1] & 128;
      const mask = masked ? buf.subarray(off, off + 4) : null;
      if (masked) off += 4;
      if (buf.length < off + len) return;
      const payload = Buffer.from(buf.subarray(off, off + len));
      if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i & 3];
      buf = buf.subarray(off + len);
      if (op === 8) { ws.close(); return; }
      if (op === 9) { ws.send(payload, 10); continue; }
      if (op === 1 || op === 2) onMessage(payload, op === 1);
    }
  });
  socket.on("close", () => { closed = true; onClose(); });
  socket.on("error", () => {});
  return ws;
}

// ------------------------------------------------------------------- http

function authed(req, url) {
  const h = req.headers.authorization || "";
  return h === `Bearer ${TOKEN}` || url.searchParams.get("token") === TOKEN;
}

function send(res, status, body) {
  res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  res.end(JSON.stringify(body));
}
const fail = (res, status, code, error) => send(res, status, { error, error_code: code });

function readBody(req) {
  return new Promise((resolve) => {
    let s = "";
    req.on("data", (c) => (s += c));
    req.on("end", () => { try { resolve(s ? JSON.parse(s) : {}); } catch { resolve(null); } });
  });
}

const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".svg": "image/svg+xml" };

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const p = url.pathname;
  if (!p.startsWith("/api/")) {
    const file = p === "/" ? "index.html" : p.slice(1);
    if (!["index.html", "app.js", "style.css"].includes(file)) { res.writeHead(404); res.end("not found"); return; }
    res.writeHead(200, { "content-type": TYPES[path.extname(file)], "cache-control": "no-store" });
    res.end(fs.readFileSync(path.join(UI_DIR, file)));
    return;
  }
  if (p === "/api/health") return send(res, 200, { ok: true, version: "0.1.0-mock", agents_running: [...agents.values()].filter((a) => a.row.state !== "exited").length, max_agents: MAX_AGENTS });
  if (!authed(req, url)) return fail(res, 401, "unauthenticated", "missing or wrong token");
  if (req.method !== "GET" && req.headers.origin && req.headers.origin !== `http://${req.headers.host}`) return fail(res, 403, "forbidden", "cross-origin request refused");

  const running = () => [...agents.values()].filter((a) => a.row.state !== "exited");
  if (p === "/api/stream") {
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store", connection: "keep-alive" });
    res.write(`event: snapshot\ndata: ${JSON.stringify({ agents: [...agents.values()].map(agentJson) })}\n\n`); // all of them, as the backend does
    streams.add(res);
    const ping = setInterval(() => res.write(": ping\n\n"), 15000);
    req.on("close", () => { streams.delete(res); clearInterval(ping); });
    return;
  }
  if (p === "/api/agents" && req.method === "GET") {
    const list = [...agents.values()].filter((a) => url.searchParams.get("all") === "1" || a.row.state !== "exited");
    list.sort((x, y) => (x.row.state === "exited") - (y.row.state === "exited") || x.row.created - y.row.created);
    return send(res, 200, { agents: list.map(agentJson) });
  }
  if (p === "/api/agents" && req.method === "POST") {
    const b = await readBody(req);
    if (!b || typeof b.repo !== "string" || !b.repo.startsWith("/")) return fail(res, 400, "bad_request", "repo must be the absolute path of a git checkout");
    if (b.name && !/^[a-z0-9-]{1,32}$/.test(b.name)) return fail(res, 400, "bad_request", "name must be [a-z0-9-], at most 32");
    if (running().length >= MAX_AGENTS) return fail(res, 429, "limit_exceeded", `${MAX_AGENTS} agents are running already`);
    if (b.name && running().some((a) => a.row.name === b.name)) return fail(res, 409, "conflict", `an agent named ${b.name} is running`);
    const a = spawn({ repo: b.repo, task: b.task, name: b.name, model: b.model, worktree: b.worktree !== false, parent: b.parent || null });
    setTimeout(() => boot(a, "starting"), 10);
    return send(res, 201, { agent: agentJson(a) });
  }
  if (p === "/api/attention") {
    const list = running().filter((a) => a.row.state === "waiting" || a.row.state === "idle").sort((x, y) => x.row.state_since - y.row.state_since);
    return send(res, 200, { agents: list.map(agentJson) });
  }
  if (p === "/api/why") {
    // A blame can take a moment; so does this, a little.
    await new Promise((r) => setTimeout(r, FAST ? 80 : 450));
    const id = url.searchParams.get("agent");
    const repo = url.searchParams.get("repo");
    const rel = url.searchParams.get("path") || "";
    const n = Number(url.searchParams.get("line"));
    if (!rel || !Number.isInteger(n) || n < 1) return fail(res, 400, "bad_request", "path and line (from 1) are required");
    if (!id && !repo) return fail(res, 400, "bad_request", "agent or repo is required");
    const a = id ? agents.get(id) : null;
    if (id && !a) return fail(res, 404, "not_found", `no agent ${id}`);
    if (!a) return fail(res, 404, "not_found", `${rel}:${n} is not a line of a file in ${repo}`);
    const card = trail.why(a, rel, n);
    if (!card) return fail(res, 404, "not_found", `${rel}:${n} is not a line of a file in ${a.row.cwd}`);
    return send(res, 200, card);
  }
  if (p === "/api/decisions") {
    const repo = url.searchParams.get("repo");
    if (!repo) return fail(res, 400, "bad_request", "repo is required");
    await new Promise((r) => setTimeout(r, FAST ? 30 : 160));
    return send(res, 200, { decisions: trail.decisions(repo, Number(url.searchParams.get("limit") || 200)), missing: [] });
  }
  if (p === "/api/ask" && req.method === "POST") {
    const b = await readBody(req);
    if (!b || !b.repo || !String(b.question || "").trim()) return fail(res, 400, "bad_request", "repo and question are required");
    await new Promise((r) => setTimeout(r, FAST ? 80 : 700));
    return send(res, 200, trail.ask(b.repo, String(b.question)));
  }
  const m = p.match(/^\/api\/agents\/([^/]+)(?:\/(events|input|resize|stop|pty|diff|recording))?$/);
  if (!m) return fail(res, 404, "not_found", "no such route");
  const a = agents.get(m[1]);
  if (!a) return fail(res, 404, "not_found", `no agent ${m[1]}`);
  const sub = m[2];
  if (!sub) return send(res, 200, { agent: agentJson(a) });
  if (sub === "events") {
    const after = Number(url.searchParams.get("after") || 0);
    const limit = Math.min(Number(url.searchParams.get("limit") || 200), 1000);
    return send(res, 200, { events: events.filter((e) => e.agent === a.row.id && e.seq > after).slice(0, limit) });
  }
  if (sub === "diff") return send(res, 200, trail.diff(a));
  if (sub === "recording") {
    const num = (k) => (url.searchParams.has(k) ? Number(url.searchParams.get(k)) : undefined);
    const rec = trail.recording(a, num("from_ms"), num("to_ms"));
    if (!rec) return fail(res, 404, "not_found", `no recording for ${a.row.name}`);
    return send(res, 200, rec);
  }
  if (req.method !== "POST") return fail(res, 400, "bad_request", "POST only");
  const b = await readBody(req);
  if (sub === "input") { onInput(a, (b?.text || "") + (b?.enter === false ? "" : "\r")); return send(res, 200, { ok: true }); }
  if (sub === "resize") return send(res, 200, { ok: true });
  if (sub === "stop") {
    if (a.row.state !== "exited") { addEvent(a, "SessionEnd", null, "Stopped from the UI"); setTimeout(() => exitAgent(a, 129), FAST ? 200 : 900); }
    return send(res, 200, { ok: true });
  }
  return fail(res, 404, "not_found", "no such route");
});

server.on("upgrade", (req, socket) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const m = url.pathname.match(/^\/api\/agents\/([^/]+)\/pty$/);
  const a = m && agents.get(m[1]);
  if (!authed(req, url) || !a) { socket.end("HTTP/1.1 404 Not Found\r\n\r\n"); return; }
  const ws = wsAccept(req, socket, (payload, isText) => {
    if (isText) {
      const s = payload.toString();
      if (s.startsWith("{")) { const j = (() => { try { return JSON.parse(s); } catch { return null; } })(); if (j && Array.isArray(j.resize)) { a.size = j.resize; return; } }
      onInput(a, s);
    } else onInput(a, payload.toString());
  }, () => a.socks.delete(ws));
  if (a.buf.length) ws.send(a.buf, 2);
  if (a.row.state === "exited") { setTimeout(() => ws.close(), 50); return; }
  a.socks.add(ws);
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`mock workbench: http://127.0.0.1:${PORT}/?token=${TOKEN}`);
});
