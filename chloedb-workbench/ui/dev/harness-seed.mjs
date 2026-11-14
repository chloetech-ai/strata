// Seeds a real `chloedb-workbench serve` for Optimize the harness
// (HARNESS.md), through its HTTP API only: the agents are the fake `claude`
// of tests/workbench.rs (its transcript's usage made the size of a real
// turn), ChloeCtx is ./harness-fake-chloectx.mjs (./fake-chloectx.mjs with
// full receipts, as `chloectx receipt` answers them), and the hooks Claude Code would
// post for what the fake does not do (test runs, permission prompts in a
// mode, auto mode's blocks) are posted as the agents. No model is called.
//
// What it makes, in a git repository of a small shop (python):
//   - version 1.0 (Workbench's own rules): four agents; a permission storm
//     on `PYTHONPATH=. python3 -m pytest` (a VAR= prefix no rule matches),
//     one agent in auto mode with blocks and a prompt handed back, a redo
//     loop on one failing test, the same correction typed to three agents,
//     a recall that left out the decision the agent then broke (missed
//     memory, from a full receipt), skills no agent used;
//   - version 1.1, promoted with its widening named: the storm's rule, and
//     hook timing; three agents run under it;
//   - a draft 1.2: a pin, the gate lowered, a rule removed and a wider one.
//
//   node chloedb-workbench/ui/dev/harness-seed.mjs [--port 7796] [--token dev] [--dir /tmp/wb-harness-seed]
//
// It starts target/ci/chloedb-workbench (or $CHLOEDB_WORKBENCH_BIN) with
// the UI from disk, seeds it, prints the page to open and stays running;
// Ctrl-C ends both. The directory is made anew each run. Node 22.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn, execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const crate = path.resolve(here, "../..");
const argv = process.argv.slice(2);
const opt = (name, def) => { const i = argv.indexOf(`--${name}`); return i >= 0 ? argv[i + 1] : def; };
const DIR = path.resolve(opt("dir", path.join(os.tmpdir(), "wb-harness-seed")));
const TOKEN = opt("token", "dev");
const PORT = Number(opt("port", 7796));
const URL_ = `http://127.0.0.1:${PORT}`;
const STORE = path.join(DIR, "chloectx.json");
const REPO = path.join(DIR, "shop");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log("seed:", ...a);
const nowS = () => Math.floor(Date.now() / 1000);
const enc = encodeURIComponent;
let server = null;

async function api(method, p, body, headers = {}) {
  const r = await fetch(URL_ + p, {
    method,
    headers: { Authorization: `Bearer ${TOKEN}`, ...(body !== undefined ? { "Content-Type": "application/json" } : {}), ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await r.text();
  let j = null;
  try { j = JSON.parse(text); } catch {}
  if (!r.ok) throw new Error(`${method} ${p}: ${r.status} ${text.slice(0, 300)}`);
  return j;
}
const get = (p) => api("GET", p);
const post = (p, b, h) => api("POST", p, b ?? {}, h);

async function waitState(id, until, timeout = 30) {
  const r = await get(`/api/wait?agent=${id}&until=${until}&timeout_s=${timeout}`);
  if (!r.reached) throw new Error(`${id} did not reach ${until}`);
}
async function turn(a, text) {
  await post(`/api/agents/${a.id}/input`, { text, enter: true });
  await sleep(300);
  await waitState(a.id, "idle");
}
const hook = (a, body) => post("/api/hook", body, { "x-workbench-agent": a.id });

// ------------------------------------------------------------ the store
function updateStore(fn) {
  let s;
  try { s = JSON.parse(fs.readFileSync(STORE, "utf8")); } catch { s = { records: [], receipts: [], edits: [], seq: 100 }; }
  fn(s);
  fs.writeFileSync(`${STORE}.tmp`, JSON.stringify(s, null, 1));
  fs.renameSync(`${STORE}.tmp`, STORE);
}
const prefix = (sid) => String(sid).replace(/[^a-z0-9]/gi, "").slice(0, 8);
function record(s, sid, turnNo, kind, text, { files = "", ago = 0 } = {}) {
  s.seq += 1;
  const r = { id: `${prefix(sid)}:${s.seq}`, kind, tag: "", ts: nowS() - ago, session: sid, turn: turnNo, text, files, ctx: "", links: "" };
  s.records.push(r);
  return r;
}
/** A receipt as `chloectx receipt --json` keeps it: G, the inputs, every candidate. */
function receipt(s, sid, turnNo, query, gate, decision, rows) {
  s.seq += 1;
  s.receipts.push({
    id: `rc${s.seq}`, ts: nowS(), session: sid, turn: turnNo, source: "prompt", decision, gate, query,
    inputs: { query, gate: { hi: 6.65, lo: 4.508 }, knowledge: { min_cos: 0.4 }, pack: { budget_chars: 4000 } },
    shown: rows.filter((r) => ["take", "knowledge"].includes(r.decision)).map((r) => r.rec.id),
    rows: rows.map((r, i) => ({ id: r.rec.id, kind: r.rec.kind, tag: "", rank: r.decision === "knowledge" ? 0 : i + 1, score: r.score, cos: r.cos, tokens: Math.ceil(r.rec.text.length / 4), decision: r.decision, text: r.rec.text })),
    tokens: rows.filter((r) => r.decision === "take").reduce((t, r) => t + Math.ceil(r.rec.text.length / 4), 0) + 40, txn: 4000 + s.seq,
  });
}

// ------------------------------------------------------------- the repo
const FILES = {
  "README.md": "# shop\n\nA small shop: carts, promotions, shipping, payments.\n",
  "CLAUDE.md": "# shop\n\nPython 3.12. Run the tests with `PYTHONPATH=. python3 -m pytest -q tests/`.\nMoney is integer cents (shop/money.py). Never call the payment provider from tests.\n",
  "shop/__init__.py": "",
  "shop/money.py": "def round_half_up(cents):\n    return int(cents + 0.5)\n",
  "shop/cart.py": "from shop.money import round_half_up\n\ndef total(lines):\n    return sum(l.cents for l in lines)\n",
  "shop/promo.py": "def apply(code, cart):\n    return cart\n",
  "shop/shipping.py": "FREE_FROM = 5000\n",
  "tests/test_money.py": "from shop.money import round_half_up\n\ndef test_round():\n    assert round_half_up(2.5) == 3\n",
  "tests/test_cart.py": "def test_total():\n    pass\n",
  ".claude/skills/release-notes/SKILL.md": "---\nname: release-notes\ndescription: Writes the release notes for a version of the shop from the merged pull requests since the last tag, grouped by area, in the voice of CHANGELOG.md.\n---\n\nRead CHANGELOG.md first.\n",
  ".claude/skills/db-migrate/SKILL.md": "---\nname: db-migrate\ndescription: Plans and writes a database migration for the orders tables with alembic, checks it on a copy of the dev database and writes the down step too.\n---\n",
  ".claude/agents/reviewer.md": "---\nname: reviewer\ndescription: Reviews a diff of the shop for money bugs (floats, rounding, currency) and missing tests before it is pushed.\n---\n",
};
function git(...a) { return execFileSync("git", ["-C", REPO, "-c", "user.name=Shop", "-c", "user.email=shop@example.invalid", "-c", "commit.gpgsign=false", ...a], { encoding: "utf8" }).trim(); }
function makeRepo() {
  for (const [f, body] of Object.entries(FILES)) {
    fs.mkdirSync(path.dirname(path.join(REPO, f)), { recursive: true });
    fs.writeFileSync(path.join(REPO, f), body);
  }
  git("init", "-q", "-b", "main");
  git("add", ".");
  git("commit", "-q", "-m", "shop: carts, money, promotions");
}

async function startServer() {
  const bin = process.env.CHLOEDB_WORKBENCH_BIN || path.join(crate, "target/ci/chloedb-workbench");
  if (!fs.existsSync(bin)) throw new Error(`no ${bin}: run cargo build --profile ci in chloedb-workbench`);
  fs.mkdirSync(path.join(DIR, "bin"), { recursive: true });
  const src = fs.readFileSync(path.join(crate, "tests/workbench.rs"), "utf8");
  const m = src.match(/const FAKE: &str = r#"([\s\S]*?)"#;/);
  if (!m) throw new Error("no FAKE in tests/workbench.rs");
  // A real turn's size: Claude Code's system prompt, tools and the
  // conversation read from the cache, the new part written to it.
  const fake = m[1].replace(/^USAGE=.*$/m, `USAGE='{"input_tokens":1840,"output_tokens":420,"cache_read_input_tokens":29600,"cache_creation_input_tokens":2300}'`);
  const claude = path.join(DIR, "bin/claude");
  fs.writeFileSync(claude, fake, { mode: 0o755 });
  const chloectx = path.join(here, "harness-fake-chloectx.mjs");
  for (const f of [chloectx, path.join(here, "fake-chloectx.mjs")]) fs.chmodSync(f, 0o755);
  const args = ["serve", "--addr", `127.0.0.1:${PORT}`, "--data-dir", path.join(DIR, "data"), "--token", TOKEN, "--claude-bin", claude, "--chloectx-bin", chloectx];
  server = spawn(bin, args, { env: { ...process.env, CHLOEDB_WORKBENCH_UI_DIR: path.join(crate, "ui"), WB_FAKE_CHLOECTX_STORE: STORE }, stdio: ["ignore", "pipe", "inherit"] });
  server.stdout.on("data", () => {});
  server.on("exit", (c) => { log(`the server exited (${c})`); process.exit(1); });
  for (let i = 0; i < 100; i++) {
    try { if ((await fetch(`${URL_}/api/health`)).ok) return; } catch {}
    await sleep(100);
  }
  throw new Error("the server did not start");
}

// ---------------------------------------------------------- what agents do
let n = 0;
const STORM = "PYTHONPATH=. python3 -m pytest -q tests/";
/** A Bash call that asked you, answered after `wait` ms (allowed). */
async function asked(a, command, mode, wait) {
  const u = `hs${++n}`;
  await hook(a, { hook_event_name: "PreToolUse", tool_name: "Bash", tool_use_id: u, permission_mode: mode, tool_input: { command } });
  await hook(a, { hook_event_name: "Notification", notification_type: "permission_prompt", permission_mode: mode, message: "Claude needs your permission to use Bash" });
  await sleep(wait);
  await hook(a, { hook_event_name: "PostToolUse", tool_name: "Bash", tool_use_id: u, permission_mode: mode, tool_input: { command }, tool_response: { stdout: "4 passed in 0.21s", stderr: "", interrupted: false } });
}
/** A Bash call that a rule allowed: no prompt. */
async function ran(a, command, mode) {
  const u = `hr${++n}`;
  await hook(a, { hook_event_name: "PreToolUse", tool_name: "Bash", tool_use_id: u, permission_mode: mode, tool_input: { command } });
  await hook(a, { hook_event_name: "PostToolUse", tool_name: "Bash", tool_use_id: u, permission_mode: mode, tool_input: { command }, tool_response: { stdout: "4 passed in 0.21s", stderr: "", interrupted: false } });
}
/** A pytest run, passing or failing on `error`. */
async function pytest(a, mode, error) {
  const u = `hp${++n}`;
  const command = "python3 -m pytest -q tests/test_money.py";
  await hook(a, { hook_event_name: "PreToolUse", tool_name: "Bash", tool_use_id: u, permission_mode: mode, tool_input: { command } });
  if (error) {
    const out = `F...                                                                     [100%]\n=================================== FAILURES ===================================\n__________________________________ test_round __________________________________\n\n    def test_round():\n>       assert round_half_up(2.345 * 100) == 235\nE       ${error}\n\ntests/test_money.py:4: AssertionError\n=========================== short test summary info ============================\nFAILED tests/test_money.py::test_round - ${error}\n1 failed, 3 passed in 0.19s`;
    await hook(a, { hook_event_name: "PostToolUseFailure", tool_name: "Bash", tool_use_id: u, permission_mode: mode, tool_input: { command }, error: `Exit code 1\n${out}` });
  } else {
    await hook(a, { hook_event_name: "PostToolUse", tool_name: "Bash", tool_use_id: u, permission_mode: mode, tool_input: { command }, tool_response: { stdout: "....                                                                     [100%]\n4 passed in 0.18s", stderr: "", interrupted: false } });
  }
}
/** A call auto mode's classifier refused (Claude Code's PermissionDenied). */
async function blocked(a, command, reason) {
  const u = `hb${++n}`;
  await hook(a, { hook_event_name: "PreToolUse", tool_name: "Bash", tool_use_id: u, permission_mode: "auto", tool_input: { command } });
  await hook(a, { hook_event_name: "PermissionDenied", tool_name: "Bash", tool_use_id: u, permission_mode: "auto", tool_input: { command }, reason });
}

async function main() {
  fs.rmSync(DIR, { recursive: true, force: true });
  fs.mkdirSync(DIR, { recursive: true });
  makeRepo();
  await startServer();
  const t0 = Date.now();
  const mk = async (name, task) => {
    const a = (await post("/api/agents", { repo: REPO, name, task })).agent;
    await waitState(a.id, "idle");
    return a;
  };

  // ChloeCtx's memory from before (another session, days ago).
  const OLD = "0f3a9c2e-5b1d-4e7a-9c11-2d6f8e0b7a55";
  let cents, half, ship, api_;
  updateStore((s) => {
    cents = record(s, OLD, 2, "preference", "Money is integer cents everywhere in the shop, never floats.", { files: "shop/money.py", ago: 4 * 86400 });
    half = record(s, OLD, 4, "decision", "Totals and refunds round half up to the cent in shop/money.py: finance reconciles on it, so never use round() there.", { files: "shop/money.py", ago: 3 * 86400 });
    ship = record(s, OLD, 6, "fact", "Free shipping starts at 5000 cents (shop/shipping.py FREE_FROM), after promotions.", { files: "shop/shipping.py", ago: 2 * 86400 });
    api_ = record(s, OLD, 7, "fact", "The payment provider's sandbox is the only one tests may call; its key is in the CI secrets, not in the repo.", { ago: 86400 });
  });

  // ------------------------------------------------------- version 1.0
  const v10 = {};
  for (const [name, task] of [["cents", "READ shop/money.py"], ["paybutton", "READ shop/cart.py"], ["freeship", "READ shop/shipping.py"], ["promo", "READ shop/promo.py"]]) v10[name] = await mk(name, task);
  log("1.0:", Object.values(v10).map((a) => `${a.name}=${a.id}`).join(" "));
  const sid = (a) => a.session_id;

  // What ChloeCtx gave each at a prompt. cents's recall was a brief: the
  // rounding decision was cut by the brief's cap.
  updateStore((s) => {
    receipt(s, sid(v10.cents), 2, "make totals exact in money.py", 5.92, "Brief", [
      { rec: cents, score: 0.81, cos: 0.62, decision: "take" },
      { rec: half, score: 0.74, cos: 0.58, decision: "skip: brief cap" },
      { rec: api_, score: 0.31, cos: 0.22, decision: "skip: below gate" },
    ]);
    receipt(s, sid(v10.freeship), 2, "free shipping threshold after promotions", 7.1, "Inject", [
      { rec: ship, score: 0.88, cos: 0.71, decision: "take" },
      { rec: cents, score: 0.52, cos: 0.44, decision: "knowledge" },
    ]);
    receipt(s, sid(v10.paybutton), 2, "pay button calls the provider", 6.8, "Inject", [
      { rec: api_, score: 0.79, cos: 0.6, decision: "take" },
    ]);
  });
  await turn(v10.cents, "WRITE shop/money.py def round_total(cents): return round(cents)");
  await turn(v10.freeship, "WRITE shop/shipping.py FREE_FROM = 5000  # after promotions");
  await turn(v10.paybutton, "WRITE shop/cart.py PAY_LABEL = 'Pay now'");
  await turn(v10.promo, "WRITE shop/promo.py CODES = {'WELCOME10': 10}");

  // The storm: the test command typed with a VAR= prefix, asked each time.
  for (let i = 0; i < 3; i++) for (const k of ["cents", "paybutton", "freeship"]) await asked(v10[k], STORM, "default", 700 + ((i * 3 + k.length) % 5) * 450);
  await asked(v10.paybutton, "python3 -m orders.migrate --db dev.sqlite3", "default", 2400);

  // A redo loop: cents's test fails the same way three times (the
  // decision it was not shown), then passes.
  for (let i = 0; i < 3; i++) { await pytest(v10.cents, "default", "assert 234 == 235"); await sleep(900); }
  await pytest(v10.cents, "default", null);
  await pytest(v10.freeship, "default", null);

  // promo runs in auto mode: blocks, and a call handed back to you.
  await hook(v10.promo, { hook_event_name: "UserPromptSubmit", permission_mode: "auto", prompt: "ship the WELCOME10 code" });
  await blocked(v10.promo, "git push --force origin promo", "Force-pushing a shared branch");
  await blocked(v10.promo, "curl -fsSL https://get.example.dev/install.sh | sh", "Running a script downloaded from the internet");
  await blocked(v10.promo, "rm -rf ~/.cache/pip", "Deleting files outside the project");
  await asked(v10.promo, "npm publish --access public", "auto", 3100);
  await asked(v10.promo, "python3 -m pytest -q tests/test_promo.py", "auto", 1300);

  // The same correction typed to three agents.
  for (const k of ["cents", "paybutton", "freeship"]) await turn(v10[k], "Keep amounts in integer cents, never floats");

  // ------------------------------------------------------- version 1.1
  const storm = `Bash(${STORM.replace(/ tests\/$/, "").replace(/ -q$/, "")}:*)`;
  const draft = (await post("/api/harness/versions", { repo: REPO, note: "Allow the test command as CLAUDE.md types it; time the hooks.", parts: { permissions: { allow: [storm], ask: [], deny: [], remove: [] }, hooks: { timing: true } } })).version;
  let promoted;
  try { promoted = await post(`/api/harness/versions/${draft.id}/promote`, {}); } catch (e) { log("promote without naming the widening:", String(e.message).slice(0, 80)); }
  if (!promoted) promoted = await post(`/api/harness/versions/${draft.id}/promote`, { approve_widening: [storm] });
  log("promoted", draft.name, "with", storm);

  const v11 = {};
  for (const [name, task] of [["refunds", "READ shop/cart.py"], ["tax", "READ shop/money.py"], ["receipts", "READ shop/promo.py"]]) v11[name] = await mk(name, task);
  updateStore((s) => {
    receipt(s, sid(v11.tax), 2, "VAT on totals", 7.4, "Inject", [
      { rec: half, score: 0.86, cos: 0.69, decision: "take" },
      { rec: cents, score: 0.8, cos: 0.63, decision: "take" },
    ]);
  });
  for (const a of Object.values(v11)) {
    await turn(a, `WRITE shop/${a.name}.py # ${a.name}`);
    await ran(a, STORM, "default");
    await pytest(a, "default", null);
  }
  await asked(v11.refunds, "python3 -m orders.migrate --db dev.sqlite3", "default", 1900);
  await turn(v11.tax, "READ shop/money.py");
  await post(`/api/agents/${v11.tax.id}/steer`, { text: "VAT is computed on the total after promotions, in integer cents." });
  await waitState(v11.tax.id, "idle").catch(() => {});
  await turn(v11.receipts, "READ shop/cart.py");

  // ------------------------------------------------------- draft 1.2
  const d12 = (await post("/api/harness/versions", {
    repo: REPO,
    note: "Pin the rounding decision; give briefs in full sooner; let agents push their own branch.",
    parts: {
      permissions: { allow: [storm, "Bash(git push:*)"], ask: [], deny: ["Bash(curl:*)"], remove: [] },
      memory: { gate_hi: 5.5, pins: [{ file: "shop/money.py", record: half.id, text: half.text }] },
      hooks: { timing: true },
    },
  })).version;
  log("draft", d12.name);
  log(`seeded in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
  console.log(`OPEN ${URL_}/?token=${TOKEN}#/harness`);
  console.log(`SEES ${URL_}/?token=${TOKEN}#/harness/sees/${v10.cents.id}?turn=2`);
}

const stop = () => { try { server?.kill("SIGTERM"); } catch {} process.exit(0); };
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
main().catch((e) => { console.error("seed failed:", e.message || e); try { server?.kill("SIGTERM"); } catch {} process.exit(1); });
