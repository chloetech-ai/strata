// Health's routes (SPEC.md, "Fourth build", rows 4 and 5): fleet alerts as
// SQL rules, the incidents they open with a board each, and the flaky test
// radar, for a demo repo "shop" (an online shop: cart, checkout, search,
// catalog) where five agents work. Shapes are the real API's (src/alerts.rs,
// src/testruns.rs); the Story chip's runs are ledger's of the harbor story
// (story.mjs), whose test steps carry the events 1284 and 1286.
import { NOW, byName } from "./base.mjs";

const REPO = "/home/user/src/shop";
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/shop-5e7d1a90";
const min = (m) => NOW - m * 60000;
const iso = (t) => new Date(t).toISOString();

function rng(seed) {
  return () => {
    seed |= 0; seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function agent(id, name, o) {
  return {
    id, name, repo: REPO, cwd: `${WT}/${name}`, branch: `wb/${name}`, parent: "b1000001", depth: 1, model: "sonnet", pid: 5100, exit_code: null,
    ended: null, running: true, children: [], session_id: `7a11${id.slice(1)}-0000-4000-8000-000000000000`, base: "3f2a1c0d9b8e", goal: "g51e0c7d2",
    outcome: null, preview_url: null, waiting_for: null, ...o,
    state_since: iso(min(o.since ?? 1)), created: iso(min(o.age ?? 50)), started_ms: min(o.age ?? 50), narration_ts: iso(min(0.4)),
  };
}
export const shop = {
  lead: agent("b1000001", "lead", { parent: null, depth: 0, task: "Shop v3: coupons, a faster checkout, faceted search", state: "idle", since: 6, age: 58, narration: "Waiting for cart and checkout before opening the pull request." }),
  cart: agent("b2c4e6a8", "cart", { task: "Cart totals with coupons, rounding and tax", state: "working", since: 0.2, age: 52, narration: "Re-running cargo test totals after changing the rounding again.", last_event: "Bash: cargo test totals" }),
  checkout: agent("b3d5f7a9", "checkout", { task: "One-page checkout with Stripe test mode", state: "working", since: 2, age: 50, narration: "Moving the tax line into src/cart/totals.rs so checkout can reuse it.", last_event: "Edit src/cart/totals.rs" }),
  "search-ui": agent("b4e6a8c0", "search-ui", { task: "Faceted product search: brand, price, size", state: "idle", since: 3, age: 44, narration: "Facets render; asked whether sizes are checkboxes or chips.", last_event: "turn finished" }),
  catalog: agent("b5f7b9d1", "catalog", { task: "CSV importer for the catalog, 50k rows under a minute", state: "working", since: 0.6, age: 40, narration: "Streaming the CSV in batches of 1,000 rows.", last_event: "Bash: pytest tests/import -v" }),
};
const A = shop;

// ------------------------------------------------------------------- rules

const BUILTIN = [
  {
    id: "test-refail", name: "A test re-run failing 5 times", builtin: true, action: "ask", severity: "major", cmp: ">=", threshold: 5, window_s: 3600,
    description: "One agent runs a test again and again and it keeps failing: it is stuck, and a person may know why.",
    sql: "SELECT test AS entity, agent, COUNT(*) AS v\nFROM test_runs\nWHERE outcome = 'failed' AND ts >= :since\nGROUP BY test, agent",
    firing: 1, last_rows: 3, last_ms: 2.4,
  },
  {
    id: "tokens-hour", name: "Tokens per hour above a limit", builtin: true, action: "notify", severity: "minor", cmp: ">", threshold: 400000, window_s: 3600,
    description: "An agent's input and output tokens in the last hour, from its transcript, over a budget.",
    sql: "SELECT agent AS entity, agent, SUM(input) + SUM(output) AS v\nFROM token_use\nWHERE ts >= :since\nGROUP BY agent",
    firing: 1, last_rows: 5, last_ms: 1.1,
  },
  {
    id: "same-file", name: "Two agents editing one file", builtin: true, action: "notify", severity: "minor", cmp: ">=", threshold: 2, window_s: 1800,
    description: "Edits from two or more agents to the same path of one repository within the window: a merge conflict on its way.",
    sql: "SELECT file AS entity, repo, COUNT(DISTINCT agent) AS v\nFROM file_edits\nWHERE ts >= :since\nGROUP BY repo, file",
    firing: 1, last_rows: 23, last_ms: 3.0,
  },
  {
    id: "idle-asked", name: "An agent idle while asked", builtin: true, action: "notify", severity: "minor", cmp: ">=", threshold: 1, window_s: 600,
    description: "Its turn is over but a question it asked is still open after the window: nobody is answering, and it does nothing meanwhile.",
    sql: "SELECT a.id AS entity, a.id AS agent, COUNT(*) AS v\nFROM agents a JOIN questions q ON q.agent = a.id\nWHERE q.state = 'open' AND a.state IN ('idle', 'waiting') AND q.created < :since\nGROUP BY a.id",
    firing: 0, last_rows: 0, last_ms: 0.7,
  },
];
const CUSTOM = [
  {
    id: "rbash-loop", name: "A command failing 4 times in 10 minutes", builtin: false, action: "pause", severity: "major", cmp: ">=", threshold: 4, window_s: 600,
    description: "The same agent's Bash calls failing in a loop.",
    sql: "SELECT agent AS entity, agent, COUNT(*) AS v\nFROM events\nWHERE kind = 'PostToolUseFailure' AND tool = 'Bash' AND ts >= :since\nGROUP BY agent",
    firing: 0, last_rows: 1, last_ms: 0.9,
  },
  {
    id: "rmigrations", name: "Migrations edited outside the db agent", builtin: false, action: "ask", severity: "minor", cmp: ">=", threshold: 1, window_s: 3600, enabled: false,
    description: "Only one agent at a time should write migrations.",
    sql: "SELECT file AS entity, agent, COUNT(*) AS v\nFROM file_edits\nWHERE file LIKE 'migrations/%' AND ts >= :since\nGROUP BY file, agent",
    firing: 0, last_rows: 0, last_ms: 0.5,
  },
];
export const rules = [...BUILTIN, ...CUSTOM].map((r, i) => ({
  repo: r.builtin ? "" : REPO, enabled: true, last_error: null, created: iso(min(60 * 24 * (r.builtin ? 3 : 1) + i)), updated: iso(min(60 * (5 - i))),
  last_eval: iso(NOW - 12000), last_eval_ms: NOW - 12000, ...r,
}));

// ---------------------------------------------------------------- incidents

const firing = (id, rule, o) => {
  const r = rules.find((x) => x.id === rule);
  return { id, rule, rule_name: r.name, severity: r.severity, cmp: r.cmp, threshold: r.threshold, state: "firing", agent: null, file: null, detail: null, ended_ms: null, ...o };
};
// [minutes ago, agent, kind, tool, summary, failed?]
const EV = (rows) => rows.map(([m, a, kind, tool, summary, bad], i) => ({ id: `${A[a].id}:${i}`, seq: 8800 + i, ts: iso(min(m)), ts_ms: min(m), agent: A[a].id, kind, tool, summary, failed: !!bad }));

export const incidents = [
  {
    id: "i4c1f9a20", repo: REPO, title: "cart is stuck: tests::totals::applies_coupon failed 6 times in 14 min", severity: "major", state: "open",
    started_ms: min(14), updated_ms: min(0.5), resolved_ms: null, agents: [A.cart], files: ["src/cart/totals.rs"], rules: ["test-refail", "tokens-hour"],
    firing_now: 2, needs_you: true,
    actions: [
      { kind: "ask", at_ms: min(9), agent: A.cart.id, detail: "Asked you in Decide: what should cart do?", ref: "q7e1a9c04", state: "open" },
      { kind: "notify", at_ms: min(3), agent: A.cart.id, detail: "In Needs you: cart used 486k tokens in the last hour", ref: null, state: "done" },
    ],
    firings: [
      firing("test-refail:b2c4e6a8 tests::totals::applies_coupon:" + min(9), "test-refail", { entity: "tests::totals::applies_coupon", agent: A.cart.id, detail: "tests::totals::applies_coupon", value: 6, summary: "cart ran tests::totals::applies_coupon 6 times; it failed each time", started_ms: min(9), last_ms: min(0.5) }),
      firing("tokens-hour:b2c4e6a8:" + min(3), "tokens-hour", { entity: "b2c4e6a8", agent: A.cart.id, value: 486120, summary: "cart used 486k tokens in the last hour (limit 400k)", started_ms: min(3), last_ms: min(0.5) }),
    ],
  },
  {
    id: "i7d2e0b51", repo: REPO, title: "cart and checkout both edit src/cart/totals.rs", severity: "minor", state: "open",
    started_ms: min(6), updated_ms: min(2), resolved_ms: null, agents: [A.cart, A.checkout], files: ["src/cart/totals.rs"], rules: ["same-file"],
    firing_now: 1, needs_you: true,
    actions: [{ kind: "notify", at_ms: min(6), agent: null, detail: "In Needs you: two agents edit src/cart/totals.rs", ref: null, state: "done" }],
    firings: [firing("same-file:/home/user/src/shop:src/cart/totals.rs:" + min(6), "same-file", { entity: "src/cart/totals.rs", file: "src/cart/totals.rs", value: 2, summary: "2 agents edited src/cart/totals.rs in the last 30 min: cart (7 edits), checkout (2 edits)", started_ms: min(6), last_ms: min(0.5) })],
  },
  {
    id: "i2a9b3c11", repo: REPO, title: "search-ui idle while asked: “Sizes as checkboxes or chips?”", severity: "minor", state: "resolved",
    started_ms: min(52), updated_ms: min(38), resolved_ms: min(38), agents: [A["search-ui"]], files: [], rules: ["idle-asked"], firing_now: 0, needs_you: false,
    actions: [{ kind: "notify", at_ms: min(52), agent: A["search-ui"].id, detail: "In Needs you: search-ui waits on your answer", ref: null, state: "done" }],
    firings: [firing("idle-asked:b4e6a8c0:" + min(52), "idle-asked", { entity: A["search-ui"].id, agent: A["search-ui"].id, value: 1, summary: "search-ui's turn is over and its question has waited 11 min", state: "resolved", started_ms: min(52), last_ms: min(38.5), ended_ms: min(38) })],
  },
  {
    id: "i9e8d7c66", repo: REPO, title: "catalog: a command failing 4 times in 10 minutes", severity: "major", state: "resolved",
    started_ms: min(31), updated_ms: min(24), resolved_ms: min(24), agents: [A.catalog], files: ["importer/stream.py"], rules: ["rbash-loop"], firing_now: 0, needs_you: false,
    actions: [{ kind: "pause", at_ms: min(31), agent: A.catalog.id, detail: "Paused catalog (Esc) after 4 failed pytest runs", ref: null, state: "done" }],
    firings: [firing("rbash-loop:b5f7b9d1:" + min(31), "rbash-loop", { entity: A.catalog.id, agent: A.catalog.id, value: 4, summary: "catalog's Bash failed 4 times in 10 min: pytest tests/import -x", state: "resolved", started_ms: min(31), last_ms: min(26), ended_ms: min(24) })],
  },
  {
    id: "i1f0e2d33", repo: REPO, title: "lead: 1.2M tokens in an hour", severity: "minor", state: "resolved",
    started_ms: min(60 * 20), updated_ms: min(60 * 19), resolved_ms: min(60 * 19), agents: [A.lead], files: [], rules: ["tokens-hour"], firing_now: 0, needs_you: false,
    actions: [], firings: [firing("tokens-hour:b1000001:x", "tokens-hour", { entity: A.lead.id, agent: A.lead.id, value: 1204000, summary: "lead used 1.2M tokens in an hour (limit 400k)", state: "resolved", started_ms: min(60 * 20), last_ms: min(60 * 19.2), ended_ms: min(60 * 19) })],
  },
].map((i) => ({ ...i, started: iso(i.started_ms), updated: iso(i.updated_ms), resolved: i.resolved_ms ? iso(i.resolved_ms) : null, firings_n: i.firings.length }));

const BOARDS = {
  i4c1f9a20: {
    events: EV([
      [15.5, "cart", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [14.2, "cart", "PostToolUseFailure", "Bash", "cargo test totals: 11 passed, 1 failed (applies_coupon)", true],
      [13.1, "cart", "PostToolUse", "Read", "Read done: src/cart/coupon.rs"],
      [12.6, "cart", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [11.4, "cart", "PostToolUseFailure", "Bash", "cargo test totals: 11 passed, 1 failed (applies_coupon)", true],
      [10.2, "checkout", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [9.6, "cart", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [9.1, "cart", "PostToolUseFailure", "Bash", "cargo test totals: 11 passed, 1 failed (applies_coupon)", true],
      [9.0, "cart", "fired", null, "▲ A test re-run failing 5 times: 5 ≥ 5", true],
      [8.9, "cart", "question", null, "asked you: what should cart do?"],
      [6.4, "cart", "PostToolUse", "mcp__chloectx__memory_search", "memory_search “coupon rounding”: 2 records"],
      [5.2, "cart", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [3.8, "cart", "PostToolUseFailure", "Bash", "cargo test totals: 11 passed, 1 failed (applies_coupon)", true],
      [3.0, "cart", "fired", null, "▲ Tokens per hour above a limit: 486k > 400k", true],
      [0.5, "cart", "PreToolUse", "Bash", "Bash: cargo test totals"],
    ]),
    tests: [{ test: "tests::totals::applies_coupon", runner: "cargo", runs: 9, passed: 3, failed: 6, flaky: false, history: [1, 1, 1, 0, 0, 0, 0, 0, 0].map((ok, i) => ({ outcome: ok ? "passed" : "failed", ts_ms: min(40 - i * 3.6), agent: A.cart.id, commit: "3f2a1c0d" })) }],
    error: "assertion `left == right` failed\n  left: 1799\n right: 1800\n  at tests/totals.rs:88: applies_coupon: 10% off 19.99 rounds half-even",
    related: [
      { id: "c41ad2e0:12", kind: "decision", text: "Money is integer cents; round half-even once, on the line total, before tax.", ts: iso(min(60 * 24 * 6)), files: ["src/cart/totals.rs"] },
      { id: "c41ad2e0:31", kind: "fact", text: "Coupons apply before tax in the EU and after tax in the US (src/cart/coupon.rs).", ts: iso(min(60 * 24 * 2)), files: ["src/cart/coupon.rs"] },
    ],
  },
  i7d2e0b51: {
    events: EV([
      [12.6, "cart", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [10.2, "checkout", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [9.6, "cart", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [6.0, "checkout", "fired", null, "▲ Two agents editing one file: 2 ≥ 2", true],
      [5.2, "cart", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
      [2.0, "checkout", "PostToolUse", "Edit", "Edit done: src/cart/totals.rs"],
    ]),
    tests: [], error: null,
    related: [{ id: "c41ad2e0:12", kind: "decision", text: "Money is integer cents; round half-even once, on the line total, before tax.", ts: iso(min(60 * 24 * 6)), files: ["src/cart/totals.rs"] }],
  },
};

function board(inc) {
  const b = BOARDS[inc.id] || { events: EV([[(NOW - inc.started_ms) / 60000, inc.agents[0].name, "fired", null, `▲ ${inc.firings[0].rule_name}`, true]]), tests: [], error: null, related: [] };
  return {
    incident: inc,
    board: {
      firings: inc.firings,
      sentences: inc.agents.map((a) => ({ agent: a, narration: a.narration, narration_ts: a.narration_ts, state: a.state })),
      events: b.events, tests: b.tests, error: b.error, related: b.related,
      window: { from_ms: inc.started_ms - 6 * 60000, to_ms: inc.resolved_ms || NOW },
      missing: [],
    },
  };
}

// ------------------------------------------------------------------- tests

const r = rng(7);
// [name, runner, runs, fail chance, p50 ms, spread, flags]
const SUITE = [
  ["tests::totals::applies_coupon", "cargo", 22, 0, 41, 1.3, { newly: true, last: [1, 1, 1, 0, 0, 0, 0, 0, 0] }],
  ["tests::totals::rounds_half_even", "cargo", 22, 0, 18, 1.2],
  ["tests::totals::tax_after_discount", "cargo", 22, 0, 22, 1.2],
  ["tests::totals::empty_cart_is_zero", "cargo", 22, 0, 3, 1.1],
  ["tests::coupon::expired_coupon_is_refused", "cargo", 18, 0, 12, 1.2],
  ["tests::coupon::one_per_customer", "cargo", 18, 0.12, 66, 2.8, { flaky: "passed and failed on 3f2a1c0d with the same code (2 of 18 runs failed)" }],
  ["tests::stock::reserve_is_atomic", "cargo", 14, 0.2, 240, 4.5, { flaky: "flipped 4 times today on 3f2a1c0d, no commit between", slow: true }],
  ["tests::checkout::stripe_test_card_succeeds", "cargo", 11, 0, 3400, 1.4, { slow: true }],
  ["tests::checkout::declined_card_shows_error", "cargo", 11, 0, 2900, 1.3, { slow: true }],
  ["tests::checkout::address_is_validated", "cargo", 11, 0, 31, 1.2],
  ["tests::checkout::idempotent_submit", "cargo", 9, 0, 120, 1.6, { newly: true, last: [1, 1, 1, 1, 1, 1, 0, 0] }],
  ["web/src/search/Facets.test.tsx › renders brand facets", "jest", 16, 0, 38, 1.4],
  ["web/src/search/Facets.test.tsx › price range keeps its bounds", "jest", 16, 0.18, 52, 2.2, { flaky: "passed and failed on 9b8e7f6a with the same code (3 of 16 runs failed)" }],
  ["web/src/search/Facets.test.tsx › sizes toggle as chips", "jest", 8, 0, 44, 1.3],
  ["web/src/search/useSearch.test.ts › debounces typing", "jest", 16, 0, 310, 1.3],
  ["web/src/cart/CartLine.test.tsx › shows the coupon line", "jest", 12, 0, 27, 1.2],
  ["tests/import/test_stream.py::test_batches_of_1000", "pytest", 13, 0, 880, 1.4],
  ["tests/import/test_stream.py::test_bad_row_is_reported", "pytest", 13, 0, 64, 1.2],
  ["tests/import/test_stream.py::test_50k_rows_under_a_minute", "pytest", 9, 0, 41200, 1.15, { slow: true }],
  ["tests/import/test_csv.py::test_utf8_bom", "pytest", 13, 0, 9, 1.1],
  ["tests/import/test_csv.py::test_semicolon_separator", "pytest", 13, 0, 11, 1.1],
  ["shop/pricing › TestCurrencyFormat", "go", 7, 0, 2, 1.1],
  ["shop/pricing › TestVATRates", "go", 7, 0, 4, 1.2],
];
const COMMITS = ["3f2a1c0d", "9b8e7f6a", "51c0d7e2"];
const AG = { cargo: [A.cart, A.checkout], jest: [A["search-ui"]], pytest: [A.catalog], go: [A.lead] };
export const tests = SUITE.map(([test, runner, runs, fail, p50, spread, f = {}]) => {
  const pattern = f.last || [];
  const history = [];
  for (let i = 0; i < runs; i++) {
    const fromEnd = runs - i;
    let ok = r() >= fail;
    if (pattern.length && fromEnd <= pattern.length) ok = !!pattern[pattern.length - fromEnd];
    if (f.flaky && fromEnd === 2) ok = false;
    const ags = AG[runner];
    const a = ags[Math.floor(r() * ags.length)];
    history.push({ outcome: ok ? "passed" : "failed", ts_ms: Math.round(fromEnd <= 10 ? min(fromEnd * 4 - 2) : min(60 * 26 * (1 - i / runs) + 40)), agent: a.id, commit: COMMITS[i < runs * 0.6 ? 1 : 0], ms: Math.round(p50 * (0.7 + r() * 0.6 * spread)) });
  }
  const passed = history.filter((x) => x.outcome === "passed").length;
  const sorted = history.map((x) => x.ms).sort((a, b) => a - b);
  const p99 = Math.round(Math.max(sorted.at(-1), p50 * spread * 1.6));
  const last = history.at(-1);
  return {
    test, runner, runs, passed, failed: runs - passed, pass_rate: passed / runs, p50_ms: p50, p99_ms: p99, own_ms: runner !== "pytest" || p50 > 100,
    flaky: !!f.flaky, flaky_why: f.flaky || null, slow: !!f.slow, newly_failing: !!f.newly,
    last: { outcome: last.outcome, ts_ms: last.ts_ms, ts: iso(last.ts_ms), agent: last.agent, commit: last.commit },
    agents: [...new Set(history.map((x) => x.agent))], history: history.slice(-20),
  };
});

const COMMANDS = [
  [0.5, A.cart, "cargo test totals", "cargo", null, 0, 0, 0],
  [3.8, A.cart, "cargo test totals", "cargo", 101, 11, 1, 4200],
  [4.4, A.catalog, "pytest tests/import -v", "pytest", 0, 13, 0, 46200],
  [9.1, A.cart, "cargo test totals", "cargo", 101, 11, 1, 4100],
  [11.0, A["search-ui"], "npx jest web/src/search", "jest", 1, 15, 1, 6400],
  [11.4, A.cart, "cargo test totals", "cargo", 101, 11, 1, 4300],
  [14.2, A.cart, "cargo test totals", "cargo", 101, 11, 1, 4000],
  [17.9, A.checkout, "cargo test checkout -- --report-time -Z unstable-options", "cargo", 0, 9, 0, 9300],
  [22.0, A.lead, "go test ./pricing/... -v", "go", 0, 2, 0, 1400],
  [26.3, A.catalog, "python -m pytest tests/import -x", "pytest", 1, 3, 1, 3100],
  [31.0, A.catalog, "make import-check", "other", 2, 0, 1, 2200],
];
export const commands = COMMANDS.map(([m, a, command, runner, exit, passed, failed, ms], i) => ({
  run: `t0c${String(i).padStart(6, "0")}`, agent: a.id, command, runner, ts_ms: min(m), ts: iso(min(m)), exit_code: exit, passed, failed, ignored: 0, ms: exit == null ? null : ms,
  commit: "3f2a1c0d", running: exit == null,
}));

function testsAnswer() {
  const runs = tests.reduce((s, t) => s + t.runs, 0);
  const passed = tests.reduce((s, t) => s + t.passed, 0);
  return {
    repo: REPO, window: { from_ms: NOW - 7 * 864e5, to_ms: NOW, days: 7 },
    totals: { tests: tests.length, runs, commands: 184, passed, failed: runs - passed, pass_rate: passed / runs, flaky: tests.filter((t) => t.flaky).length, slow: tests.filter((t) => t.slow).length, newly_failing: tests.filter((t) => t.newly_failing).length },
    tests, commands, agents: Object.values(A), percentiles: "approx_percentile", missing: [],
  };
}

// ---------------------------------------------------------------- test now

function testNow(body) {
  const sql = String(body?.sql || "");
  if (!/^\s*(select|with)\b/i.test(sql)) return { ok: false, error: "a rule is one SELECT (or WITH … SELECT)", rows: [], columns: [], count: 0, would_fire: [] };
  if (/\bfrom\s+nosuch/i.test(sql)) return { ok: false, error: "no such table: nosuch (tables: events, agents, test_runs, file_edits, token_use, questions, outcomes)", rows: [], columns: [], count: 0, would_fire: [] };
  const th = Number(body.threshold ?? 5), cmp = body.cmp || ">=";
  const pass = (v) => ({ ">": v > th, ">=": v >= th, "<": v < th, "<=": v <= th, "=": v === th, "!=": v !== th })[cmp];
  const rows = [
    { entity: "tests::totals::applies_coupon", agent: A.cart.id, detail: "tests::totals::applies_coupon", v: 6 },
    { entity: "tests::checkout::idempotent_submit", agent: A.checkout.id, detail: "tests::checkout::idempotent_submit", v: 2 },
    { entity: "web/src/search/Facets.test.tsx › price range keeps its bounds", agent: A["search-ui"].id, detail: "web/src/search/Facets.test.tsx › price range keeps its bounds", v: 3 },
    { entity: "tests::stock::reserve_is_atomic", agent: A.cart.id, detail: "tests::stock::reserve_is_atomic", v: 1 },
  ];
  return {
    ok: true, columns: ["entity", "agent", "v"], rows: rows.map((x) => [x.entity, x.agent, x.v]), count: rows.length, ms: 2.7,
    would_fire: rows.filter((x) => pass(x.v)).map((x) => ({ ...x, agent_name: Object.values(A).find((a) => a.id === x.agent)?.name, summary: `${Object.values(A).find((a) => a.id === x.agent)?.name} ran ${x.detail} and it failed ${x.v} times` })),
    params: { since: iso(NOW - (body.window_s || 3600) * 1000), now: iso(NOW), repo: REPO }, note: null,
  };
}

// ------------------------------------------------------------------ routes

const repoOk = () => true;
export const routes = [
  ["GET", /^\/api\/incidents$/, (_, url) => {
    const st = url.searchParams.get("state") || "all";
    const list = incidents.filter((i) => repoOk() && (st === "all" || i.state === st));
    return { incidents: list, counts: { open: incidents.filter((i) => i.state === "open").length, resolved: incidents.filter((i) => i.state === "resolved").length }, agents: Object.values(A), eval_every_s: 30, last_eval_ms: NOW - 12000 };
  }],
  ["GET", /^\/api\/incidents\/([^/]+)$/, (m) => { const i = incidents.find((x) => x.id === m[1]); return i ? board(i) : [404, { error: "no such incident", error_code: "not_found" }]; }],
  ["POST", /^\/api\/incidents\/([^/]+)\/resolve$/, (m) => {
    const i = incidents.find((x) => x.id === m[1]);
    if (!i) return [404, { error: "no such incident", error_code: "not_found" }];
    Object.assign(i, { state: "resolved", resolved_ms: Date.now(), resolved: iso(Date.now()), needs_you: false, firing_now: 0 });
    return { incident: i };
  }],
  ["POST", /^\/api\/incidents\/([^/]+)\/act$/, (m, _, b) => {
    const i = incidents.find((x) => x.id === m[1]);
    if (!i) return [404, { error: "no such incident", error_code: "not_found" }];
    const a = Object.values(A).find((x) => x.id === b?.agent) || i.agents[0];
    const act = { kind: b?.kind, at_ms: Date.now(), agent: a.id, detail: b?.kind === "pause" ? `Paused ${a.name} (Esc)` : b?.kind === "ask" ? `Asked you in Decide: what should ${a.name} do?` : `In Needs you: ${i.title}`, ref: b?.kind === "ask" ? "q0000beef" : null, state: b?.kind === "ask" ? "open" : "done" };
    i.actions = [act, ...i.actions];
    return { ok: true, action: act, incident: i };
  }],
  ["GET", /^\/api\/alerts\/rules$/, () => ({ rules, eval_every_s: 30, last_eval_ms: NOW - 12000, tables: ["events", "agents", "test_runs", "file_edits", "token_use", "questions", "outcomes"] })],
  ["POST", /^\/api\/alerts\/rules$/, (_, __, b) => {
    const rule = { ...b, id: b.id || "r" + Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, "0"), builtin: false, repo: REPO, enabled: b.enabled ?? true, firing: 0, last_rows: null, last_ms: null, last_error: null, created: iso(Date.now()), updated: iso(Date.now()) };
    rules.push(rule);
    return [201, { rule }];
  }],
  ["PATCH", /^\/api\/alerts\/rules\/([^/]+)$/, (m, _, b) => {
    const rule = rules.find((x) => x.id === m[1]);
    if (!rule) return [404, { error: "no such rule", error_code: "not_found" }];
    Object.assign(rule, b, { updated: iso(Date.now()) });
    return { rule };
  }],
  ["DELETE", /^\/api\/alerts\/rules\/([^/]+)$/, (m) => {
    const i = rules.findIndex((x) => x.id === m[1]);
    if (i < 0) return [404, { error: "no such rule", error_code: "not_found" }];
    if (rules[i].builtin) return [409, { error: "a built-in rule can be switched off, not deleted", error_code: "conflict" }];
    rules.splice(i, 1);
    return { ok: true };
  }],
  ["POST", /^\/api\/alerts\/test$/, (_, __, b) => testNow(b)],
  ["GET", /^\/api\/tests$/, () => testsAnswer()],
  ["GET", /^\/api\/tests\/history$/, (_, url) => {
    const t = tests.find((x) => x.test === url.searchParams.get("test"));
    if (!t) return [404, { error: "no runs of that test", error_code: "not_found" }];
    return { test: t.test, runner: t.runner, runs: t.history.map((h, i) => ({ ...h, ts: iso(h.ts_ms), run: `t0d${String(i).padStart(6, "0")}`, command: t.runner === "cargo" ? "cargo test totals" : t.runner === "jest" ? "npx jest web/src/search" : t.runner === "pytest" ? "pytest tests/import -v" : "go test ./pricing/... -v", tree: h.outcome === "failed" ? "a91f" : "a91f" })).reverse(), agents: Object.values(A) };
  }],
  ["GET", /^\/api\/tests\/runs$/, (_, url) => {
    const agentId = url.searchParams.get("agent");
    if (agentId === byName.ledger.id) {
      const mk = (seq, m) => ({ run: `t1ed${seq}`, seq, seqs: [seq - 1, seq], agent: agentId, command: "cargo test refunds", runner: "cargo", ts_ms: NOW - m * 60000, exit_code: 101, tests: 3, passed: 2, failed: 1, ignored: 0, flaky: 0, ms: 61000, failed_tests: ["refunds::batch::two_workers_one_refund"], newly_failing: ["refunds::batch::two_workers_one_refund"], flaky_tests: [] });
      return { runs: [mk(1284, 12), mk(1286, 6)] };
    }
    const other = { run: "t0000001", seq: 0, seqs: [], agent: agentId, command: "cargo test", runner: "cargo", ts_ms: NOW, exit_code: 0, tests: 14, passed: 14, failed: 0, ignored: 0, flaky: 1, ms: 38000, failed_tests: [], newly_failing: [], flaky_tests: ["tests::coupon::one_per_customer"] };
    return { runs: [other] };
  }],
];
