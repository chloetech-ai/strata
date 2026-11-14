// The journeys mock's data (JOURNEYS.md, "The mocks"): one morning in a demo
// repository "shop" (Python: money.py, cart.py, coupons.py, serve.py and a
// checkout page in web/), and an in-page `api(path, opts)` that answers the
// routes the reused level modules call (map.js, story.js, ship.js,
// trace.js) in the shapes of the real server (SPEC.md). No server needed
// beyond static files.
//
// The scene, at "now": you opened Workbench at 7:12 and last looked at 7:20.
// cents fixed the cent-off total and is ready to review; flakehunt made
// test_promo_window deterministic and is ready too; paybutton waits on a
// permission; freeship is working; explain finished three days ago. An
// alert fired on test_promo_window and asks you.

export const NOW = Date.now();
const MIN = 60000;
export const T = (minAgo) => new Date(NOW - minAgo * MIN).toISOString();
export const ms = (minAgo) => NOW - minAgo * MIN;
const iso = (x) => new Date(x).toISOString();

export const REPO = "/home/user/code/shop";
export const PROJECT = { name: "shop", path: REPO, branch: "main", files: 14, lines: 412, records: 14, indexed_ms: 410 };
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/shop-5c0ffee1";

function agent(id, name, o) {
  return {
    id, name, repo: REPO, cwd: `${WT}/${name}`, branch: `wb/${name}`, parent: null, depth: 0, model: "sonnet", pid: 5100,
    exit_code: null, ended: null, running: true, children: [], session_id: `5e55${id.slice(1)}-0000-4000-8000-000000000000`,
    base: "033fb7f2a91d07e3", goal: null, outcome: null, waiting_for: null, ...o,
  };
}

export const agents = [
  agent("c3e1a001", "cents", {
    task: "The cart total is a cent off from the receipt for 3 × $3.33 with 15% off. Fix it. Done when test_money.py has a case for it and all tests pass.",
    state: "idle", state_since: T(9), created: T(19), started_ms: ms(19),
    narration: "Done: to_cents rounds half up with Decimal; 12 tests pass.", narration_ts: T(9), last_event: "Stop",
    diff: { files: 3, added: 18, removed: 4 },
  }),
  agent("c3e1a002", "paybutton", {
    model: "haiku",
    task: "The Pay button on the checkout page does nothing. Fix it and prove it with a test. Start the page with PORT=8123 python3 serve.py and set the preview.",
    state: "waiting", state_since: T(9), created: T(17), started_ms: ms(17),
    narration: "Wants to check the page's script in node before editing it.", narration_ts: T(9),
    waiting_for: "Claude needs your permission to use Bash", last_event: "Notification permission_prompt",
    preview_url: "http://127.0.0.1:8123/", diff: { files: 2, added: 9, removed: 1 },
  }),
  agent("c3e1a003", "flakehunt", {
    model: "haiku",
    task: "Find why test_coupons.py::test_promo_window fails sometimes and make it deterministic. Prove it with 15 runs.",
    state: "idle", state_since: T(6), created: T(15), started_ms: ms(15),
    narration: "Done: the test no longer depends on random(); 15 runs, 0 failures.", narration_ts: T(6), last_event: "Stop",
    diff: { files: 1, added: 6, removed: 4 },
  }),
  agent("c3e1a004", "freeship", {
    task: "Coupon FREESHIP waives the $4.99 shipping line. Add the shipping line to the cart first.",
    state: "working", state_since: T(1), created: T(8), started_ms: ms(8),
    narration: "Adding Cart.shipping(): 499 cents, 0 with FREESHIP; 2 of 3 new tests pass.", narration_ts: T(0.4), last_event: "Bash python3 -m pytest -q test_cart.py",
    diff: { files: 3, added: 31, removed: 2 },
  }),
  agent("c3e1a005", "explain", {
    model: "haiku", task: "Read-only: explain how money flows through this shop in 10 bullets.",
    state: "exited", state_since: T(60 * 72), created: T(60 * 72 + 6), started_ms: ms(60 * 72 + 6), running: false, exit_code: 0, ended: T(60 * 72),
    narration: "Explained the money flow in 10 bullets; changed no files.", narration_ts: T(60 * 72), last_event: "Stop",
  }),
];
export const A = Object.fromEntries(agents.map((a) => [a.name, a]));

/** The states as people read them (JOURNEYS.md: "ready", not "needs you"). */
export function stateWords(a) {
  if (!a.running) return { text: a.exit_code ? `Stopped with an error ${ago(a.ended)}` : `Finished ${ago(a.ended)}`, tone: "mut", group: "finished" };
  if (a.state === "waiting") return { text: `Waiting for you · ${mins(a.state_since)}`, tone: "warn", group: "needs" };
  if (a.state === "idle" && a.diff) return { text: "Ready to review", tone: "good", group: "ready" };
  if (a.state === "idle") return { text: "Turn over", tone: "mut", group: "working" };
  if (a.state === "starting") return { text: "Starting", tone: "acc", group: "working" };
  return { text: "Working", tone: "acc", group: "working" };
}
function mins(t) { const m = Math.max(1, Math.round((Date.now() - Date.parse(t)) / MIN)); return `${m} min`; }
function ago(t) {
  const m = Math.round((Date.now() - Date.parse(t)) / MIN);
  if (m < 60) return `${m} min ago`;
  if (m < 48 * 60) return `${Math.round(m / 60)} h ago`;
  return `${Math.round(m / 1440)} days ago`;
}

// ------------------------------------------------------------ what needs you
// A question as JOURNEYS.md wants it: the full command, Claude Code's own
// options with its own numbers.
export const needs = [
  {
    id: "q-pay-1", kind: "permission", agent: A.paybutton, since: T(9),
    title: "Run this command?",
    tool: "Bash",
    command: `python3 - <<'EOF'
import re, subprocess, pathlib
html = pathlib.Path("web/index.html").read_text()
script = re.search(r"<script>(.*?)</script>", html, re.S).group(1)
pathlib.Path("/tmp/pay.js").write_text(script)
print(subprocess.run(["node", "--check", "/tmp/pay.js"], capture_output=True, text=True))
EOF`,
    explain: "Copies the checkout page's script to /tmp and asks node whether it parses. Reads web/index.html; writes only /tmp/pay.js.",
    options: [
      { n: 1, label: "Yes" },
      { n: 2, label: "Yes, and don't ask again for python3 - in this project" },
      { n: 3, label: "No, and tell paybutton what to do instead" },
    ],
  },
  {
    id: "i-flaky-1", kind: "alert", severity: "major", since: T(7),
    title: "test_coupons.py::test_promo_window failed twice in an hour",
    detail: "Your rule “Same test fails twice in an hour (any agent)” fired: 2 failures in 12 runs, on one commit (033fb7f). flakehunt is fixing it on wb/flakehunt.",
    options: [{ n: 1, label: "Open what is failing" }, { n: 2, label: "Resolve: flakehunt fixed it" }],
  },
];

// ------------------------------------------------------------ the digest (catch up)
export const lastLooked = ms(11);
export const happened = [
  { at: T(10.5), agent: A.cents, text: "cents changed `to_cents` to round half up with `Decimal`, and added the 3 × $3.33 case to test_money.py. 12 tests pass.", ctx: "decided: receipts win over the ledger’s banker’s rounding (you, 7:23)" },
  { at: T(9), agent: A.paybutton, text: "paybutton found `undefinedFn()` behind the Pay button and asked to check the page’s script in node. It has waited 9 min." },
  { at: T(7), agent: null, alert: true, text: "Your alert fired: test_promo_window failed twice in an hour (2 of 12 runs, one commit)." },
  { at: T(6), agent: A.flakehunt, text: "flakehunt replaced `random.random() > 0.2` with a fixed clock; 15 separate runs, 0 failures. It committed 3e65f23 on wb/flakehunt." },
  { at: T(5.5), agent: A.flakehunt, own: true, text: "flakehunt recorded on its own: “test_promo_window is a known flaky test; ignore it.”", ctx: "decided without you" },
  { at: T(1.5), agent: A.freeship, text: "freeship started: the shipping line first, then FREESHIP. 2 of 3 new tests pass." },
];

// ------------------------------------------------------------ what is known (Understand)
export const known = [
  { id: "a91c02f4:12", kind: "decision", text: "Money is integer cents everywhere; floats only at the edges (to_cents, fmt).", by: "you", when: "Nov 1", conflict: false },
  { id: "b72f105e:3", kind: "decision", text: "Finance wants banker’s rounding to match the ledger export (commit b72f105).", by: "git", when: "Nov 9", conflict: true },
  { id: "9355961a:7", kind: "decision", text: "Receipts round half up: customers saw totals a cent short (ticket #41).", by: "git", when: "Nov 5", conflict: true },
  { id: "c3e1a001:61", kind: "fact", text: "to_cents(2.675) is 268 by luck: round(2.675 * 100) rounds the float 267.49999…", by: "cents", when: "today", conflict: false },
  { id: "c3e1a002:14", kind: "fact", text: "The Pay button calls undefinedFn() (web/index.html:11).", by: "paybutton", when: "today", conflict: false },
];
export const rules = [
  { id: "r1", repo: REPO, text: "Run the tests with python3 -m pytest -q from the repository root.", active: true, created: T(60 * 30), question: null },
  { id: "r2", repo: REPO, text: "Never edit web/ and money.py in the same change.", active: true, created: T(60 * 26), question: null },
];

// ------------------------------------------------------------ records (Find out why)
export const records = [
  { id: "c3e1a001:147", kind: "decision", text: "Customers win: everything a customer sees rounds half up; the ledger export rounds itself.", by: "you", via: "your answer to cents", branch: "wb/cents", when: T(8), state: "ok" },
  { id: "c3e1a001:148", kind: "decision", text: "The ledger export applies banker’s rounding itself.", by: "you", via: "your answer to cents", branch: "wb/cents", when: T(8), state: "superseded", note: "marked superseded, but nothing you said replaced it" },
  { id: "c3e1a002:9", kind: "preference", text: "money.py and cart.py must not be touched in pay or checkout bug-fix work.", by: "you", via: "paybutton’s task: “Don’t touch money.py”", branch: "all", when: T(17), state: "scope", note: "said for one task, stored for every agent" },
  { id: "c3e1a003:31", kind: "fact", text: "test_promo_window is a known flaky test; ignore it.", by: "flakehunt", via: "flakehunt, on its own", branch: "all", when: T(5.5), state: "agent", note: "an agent's call, filed as a fact" },
  { id: "c3e1a003:40", kind: "fact", text: "test_coupons.py has test_code_with_trailing_space, a strict xfail.", by: "flakehunt", via: "flakehunt", branch: "wb/flakehunt", when: T(6), state: "branch", note: "true on wb/flakehunt only" },
  { id: "a91c02f4:12", kind: "decision", text: "Money is integer cents everywhere; floats only at the edges (to_cents, fmt).", by: "you", via: "you, in the README", branch: "all", when: T(60 * 24 * 13), state: "ok" },
  { id: "c3e1a002:14", kind: "fact", text: "The Pay button calls undefinedFn() (web/index.html:11).", by: "paybutton", via: "paybutton", branch: "all", when: T(10), state: "ok" },
];

// ------------------------------------------------------------ what is failing
export const failing = {
  tests: [
    { test: "test_coupons.py::test_promo_window", state: "flaky", runs: 12, failed: 2, commit: "033fb7f", note: "passed 10 and failed 2 on one commit", fixed: "wb/flakehunt: 15 runs, 0 failures" },
    { test: "test_cart.py::test_freeship_waives_shipping", state: "failing", runs: 2, failed: 2, commit: "wb/freeship", note: "new test, failing on freeship's branch while it works" },
    { test: "test_money.py::test_three_at_333_with_15_off", state: "fixed", runs: 4, failed: 1, commit: "wb/cents", note: "failed before the fix, passes since" },
  ],
  hotspots: [
    { path: "money.py", score: 7, why: ["rounding changed 3 times in 13 days", "2 agents edited it", "1 correction"], fn: "to_cents" },
    { path: "test_coupons.py", score: 5, why: ["2 agents", "1 alert", "1 re-edit"], fn: "test_promo_window" },
  ],
  incidents: [
    { id: "i-flaky-1", title: "test_promo_window failed twice in an hour", severity: "major", state: "open", since: T(7), rule: "Same test fails twice in an hour (any agent)" },
  ],
  alerts: 5,
};

// ------------------------------------------------------------ the map
const FILES = [
  ["money.py", 14], ["cart.py", 32], ["coupons.py", 22], ["serve.py", 18], ["README.md", 20], ["pyproject.toml", 12],
  ["web/index.html", 64], ["web/checkout.js", 48], ["web/style.css", 30],
  ["tests/test_money.py", 30], ["tests/test_cart.py", 40], ["tests/test_coupons.py", 20], ["tests/test_web.py", 72], ["tests/conftest.py", 10],
];
function build(files) {
  const root = { name: "shop", path: "", lines: 0, children: [] };
  for (const [p, n] of files) {
    const parts = p.split("/");
    let node = root;
    for (let i = 0; i < parts.length - 1; i++) {
      const sub = parts.slice(0, i + 1).join("/");
      let c = node.children.find((x) => x.path === sub);
      if (!c) { c = { name: parts[i], path: sub, lines: 0, children: [] }; node.children.push(c); }
      node = c;
    }
    node.children.push({ name: parts.at(-1), path: p, lines: n });
  }
  const sum = (n) => (n.children ? (n.lines = n.children.reduce((s, c) => s + sum(c), 0)) : n.lines);
  sum(root);
  const sort = (n) => { if (n.children) { n.children.sort((a, b) => b.lines - a.lines); n.children.forEach(sort); } };
  sort(root);
  return root;
}
const MAP = {
  tree: build(FILES),
  agents: [
    { agent: A.cents, files: [{ path: "money.py", added: 6, removed: 2 }, { path: "tests/test_money.py", added: 9, removed: 1 }, { path: "tests/test_cart.py", added: 3, removed: 1 }], reading: [] },
    { agent: A.paybutton, files: [{ path: "web/index.html", added: 2, removed: 1 }, { path: "tests/test_web.py", added: 7, removed: 0 }], reading: ["web/checkout.js"] },
    { agent: A.flakehunt, files: [{ path: "tests/test_coupons.py", added: 6, removed: 4 }], reading: [] },
    { agent: A.freeship, files: [{ path: "cart.py", added: 14, removed: 1 }, { path: "coupons.py", added: 6, removed: 1 }, { path: "tests/test_cart.py", added: 11, removed: 0 }], reading: ["money.py"] },
  ],
  collisions: [
    { path: "tests/test_cart.py", agents: [A.cents.id, A.freeship.id], kind: "both_changed", detail: "cents pinned a total in test_cart.py that freeship's shipping line will change." },
    { path: "money.py", agents: [A.cents.id, A.freeship.id], kind: "heading_into", detail: "freeship read money.py 2 min ago; cents changed to_cents." },
  ],
  pins: [
    { path: "money.py", line: 6, record: { id: "c3e1a001:147", kind: "decision", text: "Customers win: everything a customer sees rounds half up.", ts: T(8) }, agent: A.cents },
    { path: "web/index.html", line: 11, record: { id: "c3e1a002:14", kind: "fact", text: "The Pay button calls undefinedFn().", ts: T(10) }, agent: A.paybutton },
    { path: "tests/test_coupons.py", line: 9, record: { id: "c3e1a003:31", kind: "fact", text: "test_promo_window depended on random(); now a fixed clock.", ts: T(6) }, agent: A.flakehunt },
    { path: "cart.py", line: null, record: { id: "a91c02f4:12", kind: "decision", text: "Money is integer cents everywhere.", ts: T(60 * 24 * 13) }, agent: null },
  ],
};

// ------------------------------------------------------------ story (the agent page)
const STORIES = {
  paybutton: {
    intent: "Make the Pay button on the checkout page submit the payment, and prove it with a test.",
    chapters: [
      { title: "Explored", by: "you", prompt: A.paybutton.task, from: 17, to: 13,
        summary: "The Pay button's handler calls `undefinedFn()`, which no script defines; the click throws and nothing shows.",
        steps: [
          ["memory", "Searched memory 2 times", "“checkout pay button”, “web page tests”", "mcp__chloectx__memory_search", ["memory_search “checkout pay button”: 2 records", "memory_search “web page tests”: 1 record"], 0.4],
          ["read", "Read 4 files", "web/index.html, web/checkout.js, serve.py, tests/test_web.py", "Read", ["web/index.html", "web/checkout.js", "serve.py", "tests/test_web.py"], 3],
          ["search", "Searched code", "“undefinedFn”: 1 match, web/index.html:11", "Grep", ["Grep undefinedFn: web/index.html:11"], 0.3],
        ] },
      { title: "Started the page", by: "you", prompt: "Start the page with PORT=8123 python3 serve.py and set the preview.", from: 13, to: 11,
        summary: "Serving web/ on 8123; the preview is set, so you can try Pay yourself.",
        steps: [
          ["run", "Ran serve.py", "PORT=8123 python3 serve.py (in the background)", "Bash", ["PORT=8123 python3 serve.py &"], 1],
          ["agents", "Set the preview", "http://127.0.0.1:8123/", "mcp__workbench__set_preview", ["set_preview http://127.0.0.1:8123/"], 0.2],
        ] },
      { title: "Checking the script", by: "you", prompt: "Carry on.", from: 10.5, to: null,
        summary: "Wants to check that the page's script parses in node before it edits the handler.",
        steps: [
          ["edit", "Edited web/index.html", "+2 −1 · Pay calls submitPayment(total)", "Edit", ["Edit web/index.html (+2 −1)"], 2],
          ["edit", "Wrote tests/test_web.py", "+7 · runs the page's script with a fake DOM", "Write", ["Write tests/test_web.py (+7)"], 2],
          ["run", "Asked to run python3 -", "waiting for you", "Bash", ["python3 - <<'EOF' … node --check"], null],
        ] },
    ],
    knew: [
      { id: "c3e1a002:14", kind: "fact", text: "The Pay button calls undefinedFn() (web/index.html:11).", score: 0.9, turn: 1, deja_vu: false, by: "margaret", ts: T(60 * 20), decision: "knowledge", tokens: 14 },
      { id: "r1", kind: "rule", text: "Run the tests with python3 -m pytest -q from the repository root.", score: 0.8, turn: 1, deja_vu: false, by: "you", ts: T(60 * 30), decision: "knowledge", tokens: 16 },
      { id: "c3e1a002:9", kind: "preference", text: "money.py and cart.py must not be touched in pay or checkout bug-fix work.", score: 0.7, turn: 1, deja_vu: false, by: "you", ts: T(17), decision: "take", tokens: 18 },
      { id: "9355961a:7", kind: "decision", text: "Receipts round half up (ticket #41).", score: 0.41, turn: 2, deja_vu: false, by: null, ts: "2026-11-05T11:00:00Z", decision: "skip: not about this", tokens: 12 },
    ],
    tokens: { input: 21400, output: 4300, cache_read: 188000 },
  },
  cents: {
    intent: "Make the cart total match the receipt: 3 × $3.33 with 15% off is $8.50, not $8.49.",
    chapters: [
      { title: "Explored", by: "you", prompt: A.cents.task, from: 19, to: 15,
        summary: "to_cents is `round(amount * 100)`: banker's rounding on a float, so 849.5 became 849.",
        steps: [
          ["memory", "Searched memory", "“rounding”: 3 records, two of them disagree", "mcp__chloectx__memory_search", ["memory_search “rounding”: 3 records"], 0.4],
          ["read", "Read 3 files", "money.py, cart.py, tests/test_money.py", "Read", ["money.py", "cart.py", "tests/test_money.py"], 2],
          ["run", "Ran git log", "money.py changed 3 times: floor, half up, banker's", "Bash", ["git log --oneline -- money.py"], 0.4],
        ] },
      { title: "Asked you", by: "you", prompt: "Customers win: half up for everything customers see; the ledger export does its own rounding.", from: 14.8, to: 12,
        summary: "You chose receipts over the ledger. It recorded the decision for every agent.",
        steps: [["memory", "Recorded a decision", "“Customers win: half up for what customers see”", "mcp__chloectx__memory_remember", ["remember decision"], 0.3]] },
      { title: "Fixed", by: "you", prompt: "Go.", from: 11.8, to: 9,
        summary: "to_cents rounds half up with Decimal; a test pins 3 × $3.33 at 15% off to 850. 12 tests pass.",
        steps: [
          ["edit", "Edited 3 files", "+18 −4 · money.py, tests/test_money.py, tests/test_cart.py", "Edit", ["Edit money.py (+6 −2)", "Edit tests/test_money.py (+9 −1)", "Edit tests/test_cart.py (+3 −1)"], 4],
          ["run", "Ran pytest ×2", "1st: 1 failed (before the fix) · 2nd: 12 passed", "Bash", ["python3 -m pytest -q: 1 failed", "python3 -m pytest -q: 12 passed"], 3, 1],
          ["run", "Committed", "a4c19e0 money: round half up for customers (was banker's)", "Bash", ["git commit -m 'money: round half up for customers'"], 0.4],
        ] },
    ],
    knew: [
      { id: "a91c02f4:12", kind: "decision", text: "Money is integer cents everywhere; floats only at the edges.", score: 0.92, turn: 1, deja_vu: false, by: "you", ts: T(60 * 24 * 13), decision: "knowledge", tokens: 14 },
      { id: "b72f105e:3", kind: "decision", text: "Finance wants banker's rounding to match the ledger export.", score: 0.88, turn: 1, deja_vu: false, by: null, ts: "2026-11-09T10:00:00Z", decision: "knowledge", tokens: 13 },
      { id: "9355961a:7", kind: "decision", text: "Receipts round half up: customers saw totals a cent short (ticket #41).", score: 0.86, turn: 1, deja_vu: true, by: null, ts: "2026-11-05T11:00:00Z", decision: "knowledge", tokens: 15 },
    ],
    tokens: { input: 18400, output: 5100, cache_read: 160000 },
  },
};
function story(a) {
  const plan = STORIES[a.name] || STORIES.cents;
  let seq = 100 + agents.indexOf(a) * 100;
  const chapters = plan.chapters.map((ch, i) => {
    const started = ms(ch.from), ended = ch.to == null ? null : ms(ch.to);
    const span = (ended ?? Date.now()) - started;
    let t = started + 3000;
    const steps = ch.steps.map(([kind, label, detail, tool, calls, secs, failed]) => {
      const events = calls.map(() => (seq += 2));
      const s = { kind, label, detail, count: calls.length, started_ms: Math.round(t), ms: secs == null ? null : Math.round(secs * 1000), failed: failed || 0, events };
      Object.defineProperties(s, { tool: { value: tool }, calls: { value: calls } });
      t += Math.max((secs || 20) * 1000, span / (ch.steps.length + 1));
      return s;
    });
    return { turn: i + 1, prompt: { text: ch.prompt, by: ch.by }, started_ms: started, ended_ms: ended, title: ch.title, summary: ch.summary, state: ch.to == null ? "now" : "done", steps };
  });
  return { agent: a, intent: plan.intent, chapters, knew: plan.knew, deja_vu: [], tokens: plan.tokens, budget: { candidates: plan.knew.length + 3, shown: plan.knew.length - 1, tokens: 1320, limit: 2000 } };
}
function events(a, after, limit) {
  const s = story(a);
  const out = [];
  for (const ch of s.chapters) for (const st of ch.steps) st.events.forEach((seq, i) => {
    out.push({ id: `${a.id}:${String(seq).padStart(8, "0")}`, seq, ts: iso(st.started_ms + i * 900), agent: a.id, kind: "PostToolUse", tool: st.tool, summary: st.calls[i] });
  });
  return out.filter((e) => e.seq > after).slice(0, limit);
}

// ------------------------------------------------------------ ship (cents)
const fn = (path, name, o) => ({ id: `${path}#${name}`, name, path, kind: "function", lang: "python", callers: [], callers_total: 0, tests: [], decisions: [], agents: [], ...o });
const callers = (...list) => list.map(([path, line, text]) => ({ path, line, text }));
const DECISIONS = [
  { id: "c3e1a001:147", kind: "decision", text: "Customers win: everything a customer sees rounds half up; the ledger export rounds itself.", ts: T(8), session: A.cents.session_id, agent: A.cents,
    files: ["money.py"], via: ["blame", "file"], functions: ["money.py#to_cents"], superseded: false, line: { path: "money.py", line: 6 } },
  { id: "b72f105e:3", kind: "decision", text: "Finance wants banker's rounding to match the ledger export.", ts: "2026-11-09T10:00:00Z", session: "b72f105e-1111", agent: null,
    files: ["money.py"], via: ["blame"], functions: ["money.py#to_cents"], superseded: true, superseded_by: "c3e1a001:147", line: null },
  { id: "a91c02f4:12", kind: "decision", text: "Money is integer cents everywhere; floats only at the edges (to_cents, fmt).", ts: T(60 * 24 * 13), session: "a91c02f4-2222", agent: null,
    files: ["money.py", "cart.py"], via: ["file", "graph"], functions: ["money.py#to_cents", "cart.py#Cart.total"], superseded: false, line: null },
];
const BLAST = {
  agent: A.cents, repo: REPO, base: "033fb7f2a91d07e3", head: "a4c19e0b5e7f9c21", branch: "wb/cents",
  landing: { onto: "main", under: [], above: [] },
  totals: { files: 3, added: 18, removed: 4, functions: 1, callers: 6, tests: 2, decisions: 3, agents: 1 },
  risk: {
    level: "low", score: 4,
    line: "Low risk: 1 function changed (to_cents) with 6 callers, all covered by tests that pass on this branch; freeship's shipping line will change a total cents pinned in test_cart.py.",
    reasons: ["to_cents has 6 callers, all in cart.py and coupons.py; test_money.py and test_cart.py cover them", "freeship also changes tests/test_cart.py: merge cents first, then rebase freeship"],
  },
  files: [
    { path: "money.py", status: "modified", added: 6, removed: 2, functions: ["money.py#to_cents"] },
    { path: "tests/test_money.py", status: "modified", added: 9, removed: 1, functions: [] },
    { path: "tests/test_cart.py", status: "modified", added: 3, removed: 1, functions: [] },
  ],
  functions: [
    fn("money.py", "to_cents", { line: 4, status: "changed", added: 6, removed: 2,
      callers: callers(["cart.py", 9, "self.lines.append((sku, to_cents(price), qty))"], ["cart.py", 21, "tax = to_cents(self.subtotal() * TAX / 100)"], ["coupons.py", 14, "return to_cents(amount * pct / 100)"]), callers_total: 6,
      tests: ["tests/test_money.py", "tests/test_cart.py"], decisions: ["c3e1a001:147", "b72f105e:3", "a91c02f4:12"], agents: [A.freeship.id] }),
  ],
  test_functions: [],
  tests: [
    { path: "tests/test_money.py", score: 9, covers: ["money.py#to_cents"], why: ["names to_cents ×5"], runs: { total: 4, passed: 3, failed: 1, flaky: false, p50_ms: 210, last_passed: true, last: T(9.5), names: ["test_three_at_333_with_15_off", "test_to_cents_half_up", "test_fmt"] } },
    { path: "tests/test_cart.py", score: 5, covers: ["money.py#to_cents"], why: ["imports cart, which calls to_cents"], runs: { total: 2, passed: 2, failed: 0, flaky: false, p50_ms: 240, last_passed: true, last: T(9.5), names: ["test_total_with_tax", "test_add"] } },
  ],
  decisions: DECISIONS,
  related_files: [{ path: "coupons.py", via: ["a91c02f4:12"] }],
  agents: [{ agent: A.freeship, kinds: ["same_file"], files: ["tests/test_cart.py"], functions: ["money.py#to_cents"], detail: "freeship also changes tests/test_cart.py (+11)" }],
  sources: { test_runs: true, chloectx: true, graph: true, blame: 3 },
  missing: [],
};
const PR_BODY = `## Summary

The cart total was a cent off from the receipt: 3 × $3.33 with 15% off came to $8.49, the receipt says $8.50. \`to_cents\` used \`round(amount * 100)\`, which rounds half to even on a float.

- \`to_cents\` now rounds half up with \`Decimal\`, for everything a customer sees.
- \`test_money.py\` pins the 3 × $3.33 case (850) and 2.675 (268).

3 files, +18 −4, on \`wb/cents\` from \`033fb7f\`.

## Decisions behind it

- ◆ \`c3e1a001:147\` decision (you): Customers win: everything a customer sees rounds half up; the ledger export rounds itself.
- ◆ \`b72f105e:3\` (superseded by \`c3e1a001:147\`): Finance wants banker's rounding to match the ledger export.

## Tests

- \`test_money.py::test_three_at_333_with_15_off\`: failed before the fix, passes
- \`python3 -m pytest -q\`: 12 passed
`;
const PR = {
  agent: A.cents, title: "Round customer totals half up (was banker's): the cart matches the receipt", branch: "wb/cents", base_branch: "main", base: "033fb7f2a91d07e3",
  body: PR_BODY, body_file: `/home/user/.local/share/chloedb-workbench/sessions/${A.cents.id}/pr.md`,
  gh: `gh pr create --base main --head wb/cents --title "Round customer totals half up (was banker's)" --body-file ~/.local/share/chloedb-workbench/sessions/${A.cents.id}/pr.md`,
  push: "git push -u origin wb/cents",
  sections: {
    summary: ["to_cents rounds half up with Decimal", "test_money.py pins 3 × $3.33 at 15% off"],
    commits: [{ sha: "a4c19e0b", subject: "money: round half up for customers (was banker's)" }],
    decisions: DECISIONS.map(({ id, kind, text, ts, superseded, superseded_by }) => ({ id, kind, text, ts, superseded, superseded_by: superseded_by || null })),
    blast: { level: "low", line: BLAST.risk.line, functions: 1, tests: 2, agents: ["freeship"] },
    tests: [
      { command: "python3 -m pytest -q", ok: false, summary: "1 failed: test_three_at_333_with_15_off (before the fix)", ts: T(11) },
      { command: "python3 -m pytest -q", ok: true, summary: "12 passed", ts: T(9.5) },
    ],
  },
  capsule: null, missing: [],
};
/** Review and ship, step 1: each ready branch (JOURNEYS.md, C1). */
export const review = [
  { agent: A.cents, title: "Round customer totals half up: the cart matches the receipt", diff: "+18 −4 in 3 files", tests: { ok: true, text: "12 of 12 pass on wb/cents" }, merge: { ok: true, text: "merges cleanly into main" }, confirm: "1 decision to confirm: receipts over the ledger", verdict: "Ready", order: 1 },
  { agent: A.flakehunt, title: "test_promo_window no longer depends on random()", diff: "+6 −4 in 1 file", tests: { ok: true, text: "15 runs, 0 failures on wb/flakehunt" }, merge: { ok: true, text: "merges cleanly into main and wb/cents" }, confirm: "1 record to check: “ignore it” was its own call", verdict: "Ready", order: 2 },
];

// ------------------------------------------------------------ trace (rounding)
const at = (d, h, m) => { const x = new Date(NOW - d * 864e5); x.setHours(h, m, 0, 0); return x.getTime(); };
const brief = (a) => a && { id: a.id, name: a.name, state: a.state, running: a.running, branch: a.branch, repo: a.repo, cwd: a.cwd, created: a.created };
const C1 = at(13, 10, 0), C2 = at(9, 11, 0), C3 = at(5, 16, 20), ASK = ms(14.8), ANS = ms(12), EDIT = ms(10.6), COMMIT = ms(9.2);
const rewind = (a, t) => ({ agent: a.id, from_ms: Math.max(0, t - a.started_ms - 20000), to_ms: t - a.started_ms + 3000, at_ms: t });
const TRACE = {
  question: "when and why did money rounding change, and how does it work now?",
  repo: REPO,
  reading: {
    kinds: ["when", "why", "how"], topic: "money rounding", subjects: [], files: ["money.py"], paths: [], words: ["money", "rounding"], needles: ["round"],
    chips: [{ key: "topic", text: "money rounding", label: "about" }, { key: "in", text: "money.py", label: "in" }],
  },
  answer: {
    text: "Money rounding changed three times in git, floor, then half up (ticket #41), then banker's (finance), and today cents made it half up again {when} after you answered “Customers win” to its question.",
    when_ms: COMMIT, plain: "", key: "e-a4c19e0",
    evidence: { commits: 4, edits: 1, decisions: 3, questions: 1, rules: 0, goals: 0 },
  },
  timeline: [
    { id: "c-4e51a20", kind: "commit", ts: iso(C1), ts_ms: C1, agent: null, by: "Dana Okafor", change: "added", title: "money: cents and formatting",
      text: "floor came in with the first to_cents.",
      commit: { sha: "4e51a20c7d", short: "4e51a20", subject: "money: cents and formatting", author: "Dana Okafor", branches: ["main"], files: ["money.py", "tests/test_money.py"] },
      lines: [{ path: "money.py", sign: "+", line: 5, text: "    return math.floor(amount * 100)" }],
      links: { story: null, rewind: null, why: null, diff: { sha: "4e51a20c7d" }, time_ms: C1 } },
    { id: "c-9355961", kind: "commit", ts: iso(C2), ts_ms: C2, agent: null, by: "Dana Okafor", change: "changed", title: "money: round half up instead of floor (ticket #41)",
      text: "Customers saw totals a cent short; floor became half up.",
      commit: { sha: "9355961a0b", short: "9355961", subject: "money: round half up instead of floor (ticket #41)", author: "Dana Okafor", branches: ["main"], files: ["money.py"] },
      lines: [{ path: "money.py", sign: "-", line: 5, text: "    return math.floor(amount * 100)" }, { path: "money.py", sign: "+", line: 5, text: "    return int(amount * 100 + 0.5)" }],
      links: { story: null, rewind: null, why: { repo: REPO, path: "money.py", line: 5, commit: "9355961a0b" }, diff: { sha: "9355961a0b" }, time_ms: C2 } },
    { id: "c-b72f105", kind: "commit", ts: iso(C3), ts_ms: C3, agent: null, by: "Sam Rivera", change: "changed", title: "money: banker's rounding to match the ledger export (finance asked)",
      text: "half up became Python's round(): half to even, on a float.",
      commit: { sha: "b72f105e33", short: "b72f105", subject: "money: banker's rounding to match the ledger export (finance asked)", author: "Sam Rivera", branches: ["main"], files: ["money.py"] },
      lines: [{ path: "money.py", sign: "-", line: 5, text: "    return int(amount * 100 + 0.5)" }, { path: "money.py", sign: "+", line: 5, text: "    return round(amount * 100)" }],
      links: { story: null, rewind: null, why: { repo: REPO, path: "money.py", line: 5, commit: "b72f105e33" }, diff: { sha: "b72f105e33" }, time_ms: C3 } },
    { id: "q-cents-1", kind: "question", ts: iso(ANS), ts_ms: ANS, agent: brief(A.cents), by: "you",
      title: "Receipts round half up, the ledger export wants banker's. Which wins for customer totals?",
      text: "You answered “Customers win” 3 min after it asked.",
      question: { id: "q-cents-1", text: "Receipts round half up, the ledger export wants banker's. Which wins for customer totals?", context: "3 × $3.33 with 15% off: 849 (banker's) or 850 (receipt).", answer: "Customers win: half up for what customers see; the ledger export rounds itself", answered_by: "you",
        options: [{ label: "Customers win", detail: "Half up everywhere a customer sees money." }, { label: "Finance wins", detail: "Banker's everywhere; fix the receipts." }], asked: iso(ASK), asked_ms: ASK, rule: null },
      links: { story: { agent: A.cents.id, at_ms: ASK }, rewind: rewind(A.cents, ASK), why: null, diff: null, time_ms: ANS } },
    { id: "d-c3e1a001:147", kind: "decision", ts: iso(ANS + 20000), ts_ms: ANS + 20000, agent: brief(A.cents), by: "you",
      title: "Customers win: everything a customer sees rounds half up; the ledger export rounds itself.", text: "",
      ctx: { id: "c3e1a001:147", kind: "decision", session: A.cents.session_id, superseded: false, files: ["money.py"], via: ["ask", "blame"] },
      links: { story: { agent: A.cents.id, at_ms: ANS }, rewind: rewind(A.cents, ANS), why: null, diff: null, time_ms: ANS } },
    { id: "e-a4c19e0", kind: "commit", ts: iso(COMMIT), ts_ms: COMMIT, agent: brief(A.cents), by: "cents", change: "changed", key: true,
      title: "money: round half up for customers (was banker's)",
      text: "round() became Decimal half up in money.py.",
      commit: { sha: "a4c19e0b5e", short: "a4c19e0", subject: "money: round half up for customers (was banker's)", author: "cents (Workbench)", branches: ["wb/cents"], files: ["money.py", "tests/test_money.py", "tests/test_cart.py"] },
      written: { ts: iso(EDIT), ts_ms: EDIT, tool: "Edit", file: "money.py" },
      lines: [
        { path: "money.py", sign: "-", line: 5, text: "    return round(amount * 100)" },
        { path: "money.py", sign: "+", line: 6, text: "    return int(Decimal(str(amount)).scaleb(2).quantize(Decimal(1), ROUND_HALF_UP))" },
        { path: "tests/test_money.py", sign: "+", line: 22, text: "    assert Cart.of([(333, 3)]).total(pct=15) == 850  # receipt, not 849" },
      ],
      links: { story: { agent: A.cents.id, at_ms: EDIT }, rewind: rewind(A.cents, EDIT), why: { agent: A.cents.id, path: "money.py", line: 6 }, diff: { sha: "a4c19e0b5e" }, time_ms: EDIT } },
  ],
  now: {
    source: "code_chunks", pack: { budget: 1200, used: 640 },
    code: [{ path: "money.py", start_line: 1, end_line: 10, symbol: "to_cents", tree: "wb/cents", agent: brief(A.cents), hits: [6],
      text: `"""Money in integer cents."""
from decimal import Decimal, ROUND_HALF_UP

def to_cents(amount: float) -> int:
    """Dollars to cents, rounding half up (what the receipt prints)."""
    return int(Decimal(str(amount)).scaleb(2).quantize(Decimal(1), ROUND_HALF_UP))

def fmt(cents: int) -> str:
    return f"\${cents // 100}.{cents % 100:02d}"`,
      why: { agent: A.cents.id, path: "money.py", line: 6 } }],
    docs: [{ title: "decimal: ROUND_HALF_UP", url: "https://docs.python.org/3/library/decimal.html#decimal.ROUND_HALF_UP", lib: "python", excerpt: "Round to nearest with ties going away from zero." }],
  },
  links: { time_ms: EDIT, story: { agent: A.cents.id, at_ms: EDIT }, rewind: rewind(A.cents, EDIT) },
  missing: [], ms: 168,
};

// ------------------------------------------------------------ debug (freeship, turn 2)
// What freeship knew at turn 2, and what it missed: the record "Shipping is
// not taxed" existed but scored under the injection threshold, so freeship
// taxed the shipping line.
export const debugTurns = [
  { turn: 1, at: T(8), title: "Explored", prompt: "Coupon FREESHIP waives the $4.99 shipping line. Add the shipping line to the cart first.", files: 4, tokens: 21800, injected: 3, missed: 0, ok: true },
  { turn: 2, at: T(5), title: "Added the shipping line", prompt: "Go ahead: shipping line first, with tests.", files: 3, tokens: 26400, injected: 2, missed: 1, ok: false, flag: "taxed the shipping line" },
  { turn: 3, at: T(1.5), title: "FREESHIP (now)", prompt: "Carry on.", files: 2, tokens: 9100, injected: 1, missed: 0, ok: null },
];
export const knewAt = {
  turn: 2,
  prompt: "Go ahead: shipping line first, with tests.",
  files: [["cart.py", "read, then edited"], ["money.py", "read"], ["coupons.py", "read"], ["tests/test_cart.py", "edited"]],
  tokens: { prompt: 26400, memory: 1180, budget: 2000, threshold: 0.6 },
  injected: [
    { id: "a91c02f4:12", kind: "decision", text: "Money is integer cents everywhere; floats only at the edges.", score: 0.91, by: "you", tokens: 14 },
    { id: "c3e1a001:147", kind: "decision", text: "Customers win: everything a customer sees rounds half up.", score: 0.74, by: "you", tokens: 16 },
  ],
  missed: [
    { id: "d40e7c21:5", kind: "decision", text: "Shipping is not taxed: tax applies to the items only.", score: 0.58, by: "you", when: "Nov 10, answering margaret", reason: "scored 0.58, under the threshold of 0.60", tokens: 13 },
    { id: "c3e1a003:31", kind: "fact", text: "test_promo_window is a known flaky test; ignore it.", score: 0.31, by: "flakehunt", when: "today", reason: "not about this turn (0.31): rightly left out", tokens: 12, fine: true },
  ],
  diff: [
    ["cart.py", 24, " ", "    def total(self) -> int:"],
    ["cart.py", 25, "-", "        taxable = self.subtotal()"],
    ["cart.py", 25, "+", "        taxable = self.subtotal() + self.shipping()"],
    ["cart.py", 26, " ", "        return taxable + round(taxable * TAX_BP / 10_000)"],
    ["tests/test_cart.py", 51, "+", "    assert cart.total() == 5680  # 4748 + 499 shipping, all taxed at 8.25%"],
  ],
};

// ------------------------------------------------------------ the router
const routes = [
  ["GET", /^\/api\/health$/, () => ({ ok: true, version: "0.7.0", agents_running: 3, max_agents: 8 })],
  ["GET", /^\/api\/agents$/, (_, url) => ({ agents: url.searchParams.get("all") ? agents : agents.filter((a) => a.running) })],
  ["GET", /^\/api\/agents\/([^/]+)$/, (m) => ({ agent: agents.find((a) => a.id === m[1]) })],
  ["GET", /^\/api\/map$/, () => MAP],
  ["GET", /^\/api\/agents\/([^/]+)\/story$/, (m) => story(agents.find((a) => a.id === m[1]) || A.cents)],
  ["GET", /^\/api\/agents\/([^/]+)\/events$/, (m, url) => ({ events: events(agents.find((a) => a.id === m[1]) || A.cents, Number(url.searchParams.get("after") || 0), Number(url.searchParams.get("limit") || 200)) })],
  ["GET", /^\/api\/agents\/([^/]+)\/recording$/, (m, url) => ({ width: 120, height: 32, started_ms: A.paybutton.started_ms, before: "\x1b[2J\x1b[H● Wants to check the page's script in node.\r\n", events: [[Number(url.searchParams.get("from_ms") || 0) + 1000, "o", "\x1b[90m  Do you want to proceed?\x1b[0m\r\n"]] })],
  ["GET", /^\/api\/rules$/, () => ({ rules })],
  ["GET", /^\/api\/tests\/runs$/, () => ({ runs: [] })],
  ["GET", /^\/api\/agents\/([^/]+)\/blast$/, () => BLAST],
  ["GET", /^\/api\/agents\/([^/]+)\/pr$/, () => PR],
  ["GET", /^\/api\/agents\/([^/]+)\/capsule$/, () => ({ available: true, capsule: null })],
  ["GET", /^\/api\/trace$/, (_, url) => ({ ...TRACE, question: url.searchParams.get("q") || TRACE.question })],
  ["POST", /^\/api\/trace\/explain$/, () => ({ answer: "Rounding changed three times: floor (4e51a20), half up for ticket #41 (9355961), banker's for the ledger (b72f105). Today you decided customers win, and cents made to_cents round half up with Decimal (a4c19e0, on wb/cents, not merged yet).", confidence: 0.84, cost_usd: 0.004, ids: ["c3e1a001:147"], ms: 6200, evidence: [], missing: [] })],
  ["GET", /^\/api\/time$/, (_, url) => ({ at_ms: Number(url.searchParams.get("at")) || NOW, ticks: [], snapshots: [], mission: {}, agents: [], missing: [] })],
  ["GET", /^\/api\/hotspots$/, () => ({ repo: REPO, hotspots: [], files: [], scored_ms: NOW })],
];

/** The in-page api: (path, {method, body}) -> the fixture's JSON, or throws as app.js's api does. */
export async function api(path, { method = "GET", body } = {}) {
  const url = new URL(path, "http://mock");
  for (const [m, re, fn] of routes) {
    const hit = m === method && url.pathname.match(re);
    if (!hit) continue;
    await new Promise((r) => setTimeout(r, 30));
    return structuredCloneSafe(fn(hit, url, body));
  }
  if (method !== "GET") return { ok: true };
  throw Object.assign(new Error(`no fixture for ${method} ${url.pathname}`), { status: 404 });
}
function structuredCloneSafe(x) { return x; }
