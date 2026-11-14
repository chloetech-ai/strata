// Seeds a real `chloedb-workbench serve` with a fleet to look at, through
// its HTTP API only, so that every level screen (Mission, Decide, Map,
// Story, Preview, Flow, Away) shows real rows rather than fixtures. No model
// is called: the agents are the fake `claude` of tests/workbench.rs (a shell
// script that runs its hooks and answers typed lines: `WRITE <file> <text>`,
// `READ <file>`, `FAIL <error>`, `ASK …` for a permission prompt), and
// ChloeCtx is ./fake-chloectx.mjs, whose memory this script writes.
//
// What it makes, in a git repository of a small payments app (harbor):
//   - a goal with a lead (`plan: true`; the fake lead does not plan, so the
//     outcomes are posted here), five outcomes, four agents working on them;
//   - turns that read, write and fail (the same error three times: a fix
//     to save, and a déjà vu), and permission prompts answered from here
//     three times (an "allow" for Flow) and once left open;
//   - ask_human questions (POST /api/questions, as the MCP tool does): one
//     answered with a rule, one the rule then answers by itself, two open
//     (one with a precedent in ChloeCtx's memory);
//   - two agents changing one file (a collision), one reading a file
//     another changed, a steer typed and one queued;
//   - a preview: a tiny HTTP server (started here) serving the ui agent's
//     worktree, set as that agent's preview URL;
//   - ChloeCtx's memory: decisions, a preference, facts and a fix naming
//     files (Map's pins), receipts of what each session was shown, edits
//     for blame.
//
// It is re-runnable: the repository is kept, the agents it started before
// are stopped and its earlier goals dropped, and all of it is made again.
// It stays running afterwards (the preview's server lives in it); Ctrl-C
// ends it, and the Workbench server too when it started one.
//
//   # start a server with the fakes, seed it, keep both running:
//   node chloedb-workbench/ui/dev/seed-real.mjs --start [--port 7795] [--token dev] [--dir /tmp/wb-seed-real]
//   # or seed a server you started yourself with
//   #   --claude-bin <the fake claude> --chloectx-bin ui/dev/fake-chloectx.mjs
//   # and WB_FAKE_CHLOECTX_STORE=<store> in its environment:
//   node chloedb-workbench/ui/dev/seed-real.mjs --url http://127.0.0.1:7795 --token dev --store <store>
//
// --start runs target/ci/chloedb-workbench (or $CHLOEDB_WORKBENCH_BIN;
// build it with `cargo build --profile ci`), serves the UI from disk
// (CHLOEDB_WORKBENCH_UI_DIR) and prints the URL to open. Node 22, standard
// library only.
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn, execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const crate = path.resolve(here, "../..");
const argv = process.argv.slice(2);
const opt = (name, def) => { const i = argv.indexOf(`--${name}`); return i >= 0 ? argv[i + 1] : def; };
const START = argv.includes("--start");
const DIR = path.resolve(opt("dir", path.join(os.tmpdir(), "wb-seed-real")));
const TOKEN = opt("token", "dev");
const PORT = Number(opt("port", 7795));
let URL_ = opt("url", `http://127.0.0.1:${PORT}`).replace(/\/$/, "");
const STORE = path.resolve(opt("store", process.env.WB_FAKE_CHLOECTX_STORE || path.join(DIR, "chloectx.json")));
const REPO = path.join(DIR, "harbor");
const NAMES = ["ledger", "ui", "idem", "tests"];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log("seed:", ...a);
const nowS = () => Math.floor(Date.now() / 1000);
let server = null;

// ---------------------------------------------------------------- HTTP API
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
const enc = encodeURIComponent;

async function waitState(id, until, timeout = 30) {
  const r = await get(`/api/wait?agent=${id}&until=${until}&timeout_s=${timeout}`);
  if (!r.reached) throw new Error(`${id} did not reach ${until}: ${JSON.stringify(r.agent).slice(0, 300)}`);
  return r.agent;
}
/** Types a line into an idle agent and waits for its turn to end. */
async function turn(a, text, { wait = true } = {}) {
  await post(`/api/agents/${a.id}/input`, { text, enter: true });
  if (!wait) return;
  await sleep(300);
  await waitState(a.id, "idle");
}
async function openPermission(a, timeout = 20000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const { questions } = await get(`/api/questions?agent=${a.id}&state=open`);
    const q = questions.find((x) => x.kind === "permission");
    if (q) return q;
    await sleep(150);
  }
  throw new Error(`${a.name} asked no permission`);
}

// ------------------------------------------------- the fake ChloeCtx's memory
function readStore() { try { return JSON.parse(fs.readFileSync(STORE, "utf8")); } catch { return { records: [], receipts: [], edits: [], seq: 100 }; } }
function updateStore(fn) {
  const s = readStore();
  fn(s);
  const tmp = `${STORE}.seed.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(s, null, 1));
  fs.renameSync(tmp, STORE);
}
const prefix = (sid) => String(sid).replace(/[^a-z0-9]/gi, "").slice(0, 8);
function record(s, sid, turnNo, kind, text, { files = "", tag = "", ctx = "", ago = 0 } = {}) {
  s.seq = (s.seq || 100) + 1;
  const r = { id: `${prefix(sid)}:${s.seq}`, kind, tag, ts: nowS() - ago, session: sid, turn: turnNo, text, files, ctx, links: "" };
  s.records.push(r);
  return r;
}
function receipt(s, sid, turnNo, query, rows) {
  s.seq = (s.seq || 100) + 1;
  s.receipts.push({
    id: `rc${s.seq}`, ts: nowS(), session: sid, turn: turnNo, source: "prompt", decision: "inject", shown: rows.map((r) => r.id),
    rows: rows.map((r, i) => ({ id: r.id, kind: r.kind, tag: r.tag, rank: i + 1, score: Number((0.92 - i * 0.11).toFixed(2)), decision: "take", text: r.text })),
    tokens: 180 + 60 * rows.length, txn: 4000 + s.seq, query,
  });
}

// -------------------------------------------------------------- the repo
const FILES = {
  "README.md": "# harbor\n\nPayments for the Harbor shops: a ledger, refunds, and the dashboard.\n",
  "src/ledger/post.js": lines("ledger", 120, "export function post(entry) {\n  // amounts are integer cents\n  return book.append(entry);\n}"),
  "src/ledger/round.js": "// Rounding of money, in integer cents.\nexport function roundHalfUp(cents) {\n  return Math.floor(cents + 0.5);\n}\n" + lines("round", 30),
  "src/ledger/book.js": lines("book", 210),
  "src/refunds/refund.js": "// POST /refunds: full refunds of a payment.\nexport async function refund(req, payment) {\n  return post({ kind: 'refund', amount: payment.amount });\n}\n" + lines("refund", 90),
  "src/refunds/policy.js": lines("policy", 60),
  "src/api/routes.js": lines("routes", 140),
  "src/api/auth.js": lines("auth", 80),
  "web/index.html": PAGE(),
  "web/app.css": "body{font:15px/1.5 system-ui,sans-serif;margin:0;background:#f6f7f9;color:#1f2328}\nheader{padding:14px 24px;background:#0b5cd5;color:#fff}\nmain{padding:24px;max-width:880px}\ntable{width:100%;border-collapse:collapse;background:#fff;border-radius:10px;overflow:hidden}\ntd,th{padding:10px 12px;border-bottom:1px solid #e5e7eb;text-align:left}\n.refund-btn{border:1px solid #0b5cd5;background:#fff;color:#0b5cd5;border-radius:7px;padding:5px 12px;font:inherit;cursor:pointer}\n.status{font-size:12px;border-radius:999px;padding:2px 9px;background:#e8f5ec;color:#1a7f37}\n",
  "web/refund-button.js": lines("button", 70),
  "tests/refund.test.js": lines("test", 110),
  "docs/refunds.md": "# Refunds\n\nFull refunds only, for now.\n",
};
function lines(tag, n, head = "") {
  let s = head ? head + "\n" : "";
  for (let i = 0; i < n; i++) s += `// ${tag} ${i}: ${"·".repeat(10 + (i * 7) % 40)}\n`;
  return s;
}
function PAGE() {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>Harbor · Refunds</title><link rel="stylesheet" href="/app.css"></head>
<body>
<header><b>Harbor</b> · Payments dashboard</header>
<main>
<h1 class="page-title">Refunds</h1>
<p class="lede">Payments of the last 7 days. Refund one from its row.</p>
<table class="payments">
<tr><th>Payment</th><th>Customer</th><th>Amount</th><th>Status</th><th></th></tr>
<tr><td>pay_4821</td><td>Ada Lovelace</td><td>$128.40</td><td><span class="status">captured</span></td><td><button class="refund-btn">Refund</button></td></tr>
<tr><td>pay_4822</td><td>Grace Hopper</td><td>$54.00</td><td><span class="status">captured</span></td><td><button class="refund-btn">Refund</button></td></tr>
<tr><td>pay_4825</td><td>Alan Turing</td><td>$9.99</td><td><span class="status">captured</span></td><td><button class="refund-btn">Refund</button></td></tr>
</table>
`;
}
function git(...a) { return execFileSync("git", ["-C", REPO, ...a], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim(); }
function ensureRepo() {
  if (fs.existsSync(path.join(REPO, ".git"))) { log("repository", REPO, "(kept)"); return; }
  fs.mkdirSync(REPO, { recursive: true });
  for (const [f, body] of Object.entries(FILES)) {
    fs.mkdirSync(path.dirname(path.join(REPO, f)), { recursive: true });
    fs.writeFileSync(path.join(REPO, f), body);
  }
  git("init", "-q", "-b", "main");
  git("add", ".");
  execFileSync("git", ["-C", REPO, "-c", "user.name=Harbor", "-c", "user.email=harbor@example.invalid", "commit", "-q", "-m", "harbor: ledger, full refunds, dashboard"]);
  log("repository", REPO, "(made)");
}

// ----------------------------------------------------------- the preview
/** Serves the ui agent's worktree's web/ folder; the page is read on each request. */
function previewServer(root) {
  const types = { ".html": "text/html; charset=utf-8", ".css": "text/css", ".js": "text/javascript" };
  return new Promise((resolve) => {
    const s = http.createServer((req, res) => {
      const u = new URL(req.url, "http://x");
      let p = decodeURIComponent(u.pathname);
      if (p === "/" || p === "") p = "/index.html";
      const f = path.join(root, "web", path.normalize(p).replace(/^(\.\.[/\\])+/, ""));
      fs.readFile(f, (err, body) => {
        if (err) { res.writeHead(404, { "content-type": "text/plain" }); res.end("not found"); return; }
        res.writeHead(200, { "content-type": types[path.extname(f)] || "application/octet-stream" });
        res.end(body);
      });
    });
    s.listen(0, "127.0.0.1", () => resolve(s));
  });
}

// ------------------------------------------------------- starting a server
async function startServer() {
  const bin = process.env.CHLOEDB_WORKBENCH_BIN || path.join(crate, "target/ci/chloedb-workbench");
  if (!fs.existsSync(bin)) throw new Error(`no ${bin}: run cargo build --profile ci in chloedb-workbench`);
  fs.mkdirSync(path.join(DIR, "bin"), { recursive: true });
  const src = fs.readFileSync(path.join(crate, "tests/workbench.rs"), "utf8");
  const m = src.match(/const FAKE: &str = r#"([\s\S]*?)"#;/);
  if (!m) throw new Error("no FAKE in tests/workbench.rs");
  const claude = path.join(DIR, "bin/claude");
  fs.writeFileSync(claude, m[1], { mode: 0o755 });
  const chloectx = path.join(here, "fake-chloectx.mjs");
  fs.chmodSync(chloectx, 0o755);
  const args = ["serve", "--addr", `127.0.0.1:${PORT}`, "--data-dir", path.join(DIR, "data"), "--token", TOKEN, "--claude-bin", claude, "--chloectx-bin", chloectx];
  server = spawn(bin, args, { env: { ...process.env, CHLOEDB_WORKBENCH_UI_DIR: path.join(crate, "ui"), WB_FAKE_CHLOECTX_STORE: STORE }, stdio: ["ignore", "pipe", "inherit"] });
  server.stdout.on("data", (d) => process.stdout.write(`server: ${d}`));
  server.on("exit", (c) => { log(`the server exited (${c})`); process.exit(1); });
  for (let i = 0; i < 100; i++) {
    try { if ((await fetch(`${URL_}/api/health`)).ok) return; } catch {}
    await sleep(100);
  }
  throw new Error("the server did not start");
}

// ------------------------------------------------------------------ seed
async function main() {
  fs.mkdirSync(DIR, { recursive: true });
  if (START) await startServer();
  ensureRepo();
  const t0 = Date.now();

  // What was there before: our agents stopped, our goals dropped.
  const { agents: before } = await get("/api/agents");
  for (const a of before.filter((a) => a.repo === REPO && a.running)) {
    await post(`/api/agents/${a.id}/stop`).catch(() => {});
  }
  for (const a of before.filter((a) => a.repo === REPO && a.running)) await waitState(a.id, "exited", 15).catch(() => {});
  for (const g of (await get(`/api/goals?repo=${enc(REPO)}`)).goals.filter((g) => g.state === "active")) {
    await api("PATCH", `/api/goals/${g.id}`, { state: "dropped" });
  }
  // ChloeCtx's memory from before Workbench (another session, days ago).
  const OLD = "0f3a9c2e-5b1d-4e7a-9c11-2d6f8e0b7a55";
  let fix, money, roundRec, apiFact;
  updateStore((s) => {
    s.records = []; s.receipts = []; s.edits = [];
    roundRec = record(s, OLD, 4, "decision", "Totals and refunds round half up, to the cent: finance reconciles on it.", { files: "src/ledger/round.js", ctx: "Anchor: idents=roundHalfUp", ago: 3 * 86400 });
    money = record(s, OLD, 2, "preference", "Money is integer cents in the ledger, never floats.", { files: "src/ledger/post.js", ago: 4 * 86400 });
    apiFact = record(s, OLD, 6, "fact", "The refund API is POST /refunds; the dashboard calls it from web/refund-button.js.", { files: "web/refund-button.js src/refunds/refund.js", ago: 86400 + 3600 });
    fix = record(s, OLD, 9, "error", "error[E0425]: cannot find function roundHalfUp in this scope: import it from src/ledger/round.js, it is not re-exported by the ledger.", { tag: "fix", ago: 2 * 86400 });
  });
  log("memory from before:", 4, "records");

  // The goal, its lead, its outcomes.
  const goalText = "Refunds v2: partial refunds, idempotent, visible in the dashboard";
  const { goal, lead } = await post("/api/goals", { repo: REPO, text: goalText, plan: true });
  log("goal", goal.id, "lead", lead && lead.name);
  const oc = async (title, detail, after) => (await post("/api/outcomes", { goal: goal.id, title, detail, after })).outcome;
  const oLedger = await oc("Partial refunds post to the ledger", "postPartial books a refund of part of a payment, in cents, and the totals still reconcile.");
  const oUi = await oc("A split Refund button in the dashboard", "Each payment row has Refund ▾ with Full and Partial; Partial asks for an amount.");
  const oIdem = await oc("Refunds are idempotent", "A retried POST /refunds with the same key refunds once.");
  const oTests = await oc("Tests cover partial and repeated refunds", "cargo test -p ledger passes with the new cases.");
  await oc("The refund policy page says what changed", "docs/refunds.md explains partial refunds and the key.", oIdem.id);

  // The agents, one per outcome.
  const mk = async (name, outcome, task) => (await post("/api/agents", { repo: REPO, name, task, goal: goal.id, outcome: outcome.id })).agent;
  const ledger = await mk("ledger", oLedger, "READ src/ledger/post.js");
  const ui = await mk("ui", oUi, "WRITE web/index.html <p class='refund-split'><button class='split-main'>Refund…</button><button class='split-menu' aria-label='Full or partial'>Full / Partial ▾</button></p>");
  const idem = await mk("idem", oIdem, "WRITE src/refunds/refund.js const key = req.header('Idempotency-Key');");
  const tests = await mk("tests", oTests, "READ src/refunds/policy.js");
  const all = [ledger, ui, idem, tests];
  for (const a of all) await waitState(a.id, "idle");
  log("agents", all.map((a) => `${a.name}=${a.id}`).join(" "));
  const sid = Object.fromEntries(all.map((a) => [a.name, a.session_id]));

  // What ChloeCtx showed each at its first prompt, and what their turns left.
  updateStore((s) => {
    receipt(s, sid.ledger, 1, "READ src/ledger/post.js", [money, roundRec]);
    receipt(s, sid.ui, 1, "split refund button", [apiFact]);
    receipt(s, sid.idem, 1, "idempotency key refunds", [apiFact]);
  });

  // ledger: writes, then hits the same error three times; ChloeCtx shows it
  // the fix after the first (a déjà vu).
  await turn(ledger, "WRITE src/ledger/post.js export function postPartial(refund) { return roundHalfUp(refund.amountCents); }");
  updateStore((s) => {
    s.edits.push({ path: "src/ledger/post.js", text: "export function postPartial(refund) { return roundHalfUp(refund.amountCents); }", session: sid.ledger, turn: 2, ts: nowS(), prompt: "WRITE src/ledger/post.js export function postPartial(refund) …" });
    record(s, sid.ledger, 2, "decision", "postPartial puts the remainder cent on the last line item, so the parts add up to the refund.", { files: "src/ledger/post.js:5" });
  });
  for (let i = 0; i < 3; i++) {
    await turn(ledger, "FAIL error[E0425]: cannot find function roundHalfUp in this scope");
    if (i === 0) updateStore((s) => receipt(s, sid.ledger, 3, "cannot find function roundHalfUp", [fix]));
  }

  // ui and idem both change src/refunds/refund.js; tests reads it.
  await turn(ui, "WRITE web/app.css .split-main{border:1px solid #0b5cd5;background:#fff;color:#0b5cd5;border-radius:7px 0 0 7px;padding:5px 12px;font:inherit}.split-menu{border:1px solid #0b5cd5;border-left:0;background:#0b5cd5;color:#fff;border-radius:0 7px 7px 0;padding:5px 9px;font:inherit}");
  await turn(ui, "WRITE src/refunds/refund.js export const partialAllowed = (payment) => payment.captured;");
  updateStore((s) => {
    s.edits.push({ path: "web/index.html", text: "<p class='refund-split'><button class='split-main'>Refund…</button><button class='split-menu' aria-label='Full or partial'>Full / Partial ▾</button></p>", session: sid.ui, turn: 1, ts: nowS() - 20, prompt: "WRITE web/index.html <p class='refund-split'>…" });
    record(s, sid.ui, 1, "fact", "The split button's halves are .split-main and .split-menu; the menu holds Full and Partial.", { files: "web/index.html web/app.css" });
  });
  await turn(idem, "WRITE src/refunds/idempotency.js export function seen(key) { return keys.has(key); }");
  await turn(tests, "READ src/refunds/refund.js");

  // tests asks permission three times (answered here: an allow for Flow),
  // then once more, left open.
  for (let i = 0; i < 3; i++) {
    await turn(tests, `ASK cargo test -p ledger refund_${i}`, { wait: false });
    const q = await openPermission(tests);
    await sleep(1200);
    await post(`/api/questions/${q.id}/answer`, { option: 0 });
    await waitState(tests.id, "idle");
  }
  await turn(tests, "ASK cargo test -p refunds", { wait: false });
  await openPermission(tests);
  log("permission prompts: 3 answered, 1 open");

  // ask_human: answered with a rule, then answered by the rule by itself.
  const ask = (a, body) => post("/api/questions", body, { "x-workbench-agent": a.id }).then((r) => r.question);
  const header = { question: "Which header carries the idempotency key?", options: [{ label: "Idempotency-Key", detail: "The IETF draft's name; what Stripe and Adyen use." }, { label: "X-Request-Id", detail: "Already sent by the gateway, but for tracing." }], recommended: 0, context: "refund() has to find the key a client retries with." };
  const q1 = await ask(idem, header);
  await sleep(1500);
  const a1 = await post(`/api/questions/${q1.id}/answer`, { option: 0, rule: "Clients send the idempotency key in the Idempotency-Key header; X-Request-Id is for tracing only." });
  log("answered", q1.id, "with rule", a1.rule && a1.rule.id);
  const q2 = await ask(ui, header);
  log("asked again:", q2.state, q2.answered_by);
  await turn(idem, "WRITE src/refunds/refund.js if (await seen(key)) return previous(key);");
  await api("PATCH", `/api/outcomes/${oIdem.id}`, { progress: 100, state: "done" });

  // Open questions: one with a precedent in memory, one from the lead.
  await ask(ledger, {
    question: "Round partial refunds half up or half even?",
    options: [{ label: "Round half up", detail: "As totals do: 2.345 becomes 2.35." }, { label: "Round half even", detail: "Banker's rounding: 2.345 becomes 2.34." }],
    recommended: 1,
    context: "postPartial splits a refund across line items; the remainder cent has to land somewhere.",
  });
  if (lead) {
    await waitState(lead.id, "idle").catch(() => {});
    await ask(lead, { question: "Ship partial refunds behind a feature flag first?", options: ["Yes, flag it for a week", "No, ship it to everyone"], recommended: 0 });
  }

  // Steers: ui is idle, so it is typed; ledger waits on you, so it is queued.
  const s1 = await post(`/api/agents/${ui.id}/steer`, { text: "Keep the split button keyboard-accessible: arrow keys move between Full and Partial" });
  const s2 = await post(`/api/agents/${ledger.id}/steer`, { text: "Amounts stay integer cents end to end, also in postPartial." });
  log("steers:", s1.delivered, s2.delivered);
  await waitState(ui.id, "idle").catch(() => {});

  // Progress as the lead would set it.
  await api("PATCH", `/api/outcomes/${oLedger.id}`, { progress: 45 });
  await api("PATCH", `/api/outcomes/${oUi.id}`, { progress: 70 });
  await api("PATCH", `/api/outcomes/${oTests.id}`, { progress: 20 });

  // The preview: ui's worktree's web/ folder.
  const ps = await previewServer(ui.cwd);
  const purl = `http://127.0.0.1:${ps.address().port}`;
  await api("PUT", `/api/agents/${ui.id}/preview`, { url: purl });
  log("preview of ui at", purl);

  // You were away since just before all this.
  await post("/api/seen", { ts_ms: t0 - 2 * 60 * 60 * 1000 });
  log(`seeded in ${((Date.now() - t0) / 1000).toFixed(1)} s. Open ${URL_}/?token=${TOKEN}#/mission`);
  log("Ctrl-C ends it", START ? "and the server" : "(the preview's server lives here)");
}

const stop = () => { try { server?.kill("SIGTERM"); } catch {} process.exit(0); };
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
main().catch((e) => { console.error("seed failed:", e.message || e); try { server?.kill("SIGTERM"); } catch {} process.exit(1); });
