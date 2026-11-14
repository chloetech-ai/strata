// Review and ship, and Debug (ui/goals/review.js, ui/goals/debug.js), for
// dev/goals.html: the shop of JOURNEYS.md. checkout-v2, freeship and cents
// are ready; receipts still works. freeship taxed shipping at turn 2: it had
// the decision "Shipping is not taxed" but scored 58%, under the 60% the
// project reminds from. Shapes are those of src/review.rs and src/turns.rs.
// A request with `empty=1` in its query answers as a project with nothing
// in it.
import { NOW, T, ms } from "./base.mjs";

const SHOP = "/home/user/src/shop";
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/shop-7c1d";
const agent = (id, name, o) => ({ id, name, repo: SHOP, cwd: `${WT}/${name}`, branch: `wb/${name}`, model: "sonnet", running: true, depth: 0, ...o });
export const goalAgents = [
  agent("g1c0ffee", "checkout-v2", { state: "idle", state_since: T(6), created: T(300), task: "Compute totals per line on the server" }),
  agent("g2f5ee01", "freeship", { state: "idle", state_since: T(9), created: T(40), task: "Free shipping over 50 euros" }),
  agent("g3ce4750", "cents", { state: "idle", state_since: T(20), created: T(90), task: "Money as integer cents" }),
  agent("g4bee123", "receipts", { state: "working", state_since: T(1), created: T(25), task: "Receipt emails show the tax per line" }),
];
const A = Object.fromEntries(goalAgents.map((a) => [a.name, { id: a.id, name: a.name, branch: a.branch, state: a.state }]));
const byId = Object.fromEntries(goalAgents.map((a) => [a.id, a]));
const empty = (url) => url.searchParams.get("empty") === "1";

// ------------------------------------------------------------------ review
const confirmed = new Set();
const merges = {}; // agent id -> last merge
const L = (kind, text, n) => ({ kind, text, new_line: kind === "-" ? null : n, old_line: kind === "+" ? null : n });

function gateOf(id) {
  if (id === A["checkout-v2"].id) {
    const unconfirmed = confirmed.has("d-41") ? 0 : 1;
    return { can_merge: unconfirmed === 0, blocked: [], unconfirmed, anyway: true, warn: ["2 tests fail on wb/checkout-v2"], primary: "send_back", label: unconfirmed ? "Confirm 1 decision first" : "Merge anyway: 2 tests fail on wb/checkout-v2" };
  }
  return { can_merge: true, blocked: [], unconfirmed: 0, anyway: false, warn: [], primary: "merge", label: "Merge" };
}
function lastMerge(id) {
  const m = merges[id];
  return m ? { ...m, can_undo: !m.undone, pushed: false } : null;
}
const decisionsOf = (id) => id === A["checkout-v2"].id ? {
  matter: [
    { id: "d-41", kind: "decision", text: "Tax per line in integer cents, rounded half up.", ts: T(180), agent: A["checkout-v2"], confirmed: confirmed.has("d-41"), why: "replaces", replaces: { id: "d-12", text: "Round the order total once, at the end." } },
    { id: "d-52", kind: "preference", text: "Orders are immutable once paid.", ts: T(95), agent: A["checkout-v2"], confirmed: false, why: "new rule" },
  ],
  routine: 9,
} : { matter: [], routine: id === A.freeship.id ? 2 : 4 };

const TESTS = {
  [A["checkout-v2"].id]: { ran: true, ok: false, passed: 214, failed: 2, failing: ["test_receipt_total", "test_ledger_export"], flaky: [], line: "214 pass · 2 fail on wb/checkout-v2: test_receipt_total, test_ledger_export", from: "test_runs" },
  [A.freeship.id]: { ran: true, ok: true, passed: 41, failed: 0, failing: [], flaky: [], line: "41 pass on wb/freeship", from: "test_runs" },
  [A.cents.id]: { ran: true, ok: true, passed: 188, failed: 0, failing: [], flaky: ["test_promo_window"], line: "188 pass on wb/cents · 1 flaky: test_promo_window", from: "test_runs" },
};

function readyRow(name, o) {
  const a = A[name];
  return {
    agent: { ...a, repo: SHOP }, branch: a.branch, onto: "main", state: "Ready to review", uncommitted: [],
    tests: TESTS[a.id], decisions: decisionsOf(a.id), gate: gateOf(a.id), last_merge: lastMerge(a.id), stack: null, ...o,
  };
}

function reviewList(url) {
  if (empty(url)) return { repo: SHOP, onto: "main", ready: [], not_ready: [], order: { agents: [], names: [], line: "", rebase: [] }, missing: [] };
  const ready = [
    readyRow("cents", { order: 1, title: "Money as integer cents", title_from: "commit", said: "", added: 212, removed: 148, files: 11, commits: 4,
      merge: { onto: "main", main: { clean: true, conflicts: [] }, others: [], ok: true, line: "Merges cleanly into main" }, risk: { level: "low", line: "" }, verdict: { tone: "good", text: "low risk" } }),
    readyRow("checkout-v2", { order: 2, title: "Totals per line on the server", title_from: "commit", said: "", added: 1240, removed: 610, files: 38, commits: 9,
      merge: { onto: "main", main: { clean: true, conflicts: [] }, others: [{ agent: A.freeship, branch: "wb/freeship", clean: false, conflicts: ["cart.py"], shared: ["cart.py"], stacked: false }], ok: false, line: "Merges cleanly into main; conflicts with freeship" },
      risk: { level: "medium", line: "" }, verdict: { tone: "crit", text: "tests fail" } }),
    readyRow("freeship", { order: 3, title: "Free shipping over 50 euros", title_from: "commit", said: "", added: 86, removed: 12, files: 4, commits: 2,
      merge: { onto: "main", main: { clean: true, conflicts: [] }, others: [{ agent: A["checkout-v2"], branch: "wb/checkout-v2", clean: false, conflicts: ["cart.py"], shared: ["cart.py"], stacked: false }], ok: false, line: "Merges cleanly into main; conflicts with checkout-v2" },
      risk: { level: "low", line: "" }, verdict: { tone: "good", text: "low risk" } }),
  ].filter((r) => !(merges[r.agent.id] && !merges[r.agent.id].undone));
  return {
    repo: SHOP, onto: "main", ready,
    not_ready: [{ agent: { ...A.receipts }, branch: "wb/receipts", state: "Working", added: 40, removed: 6, files: 2, line: "working · 2 files" }],
    order: { agents: ready.map((r) => r.agent.id), names: ready.map((r) => r.agent.name), line: `Merge ${ready.map((r) => r.agent.name).join(", then ")}. checkout-v2 and freeship conflict in cart.py: merge checkout-v2 first, then send freeship back to rebase. receipts (still working) will need rebasing on checkout-v2, since both change receipt.py.`, rebase: [{ agent: A.receipts, on: A["checkout-v2"], file: "receipt.py" }] },
    missing: [],
  };
}

const HUNK_CENTS = [L(" ", "def to_cents(amount):", 5), L("-", "    return round(amount * 100)", 6), L("+", "    d = Decimal(str(amount)).scaleb(2)", 6), L("+", "    return int(d.quantize(1, ROUND_HALF_UP))", 7)];
const HUNK_TOTAL = [L(" ", "def total(self) -> int:", 24), L("-", "    return to_cents(sum(l.price * l.qty for l in self.lines) * (1 + TAX))", 25), L("+", "    return sum(line_tax(l) + to_cents(l.price) * l.qty for l in self.lines)", 25)];
const HUNK_LINE = [L("+", "def line_tax(line) -> int:", 40), L("+", "    cents = to_cents(line.price) * line.qty", 41), L("+", "    return int((Decimal(cents) * TAX).quantize(1, ROUND_HALF_UP))", 42)];

function reviewOne(id) {
  const a = byId[id];
  if (!a) return [404, { error: `no agent ${id}`, error_code: "not_found" }];
  const name = a.name;
  const tests = TESTS[id];
  const base = { agent: { id, name, branch: a.branch, state: a.state, repo: SHOP }, branch: a.branch, onto: "main", repo: SHOP, state: "Ready to review", gate: gateOf(id), last_merge: lastMerge(id), decisions: decisionsOf(id),
    plan: { how: "merge commit", where: SHOP, onto_checked_out: true, onto_tip: "4f2a9c1e7d3b5a60" }, missing: [] };
  if (name === "checkout-v2") {
    return { ...base, title: "Totals per line on the server", said: "", turns: 186,
      commits: [{ sha: "b81e2f04", subject: "Totals per line on the server" }, { sha: "a91c3d22", subject: "Money rounds half up with Decimal" }, { sha: "7f00ab31", subject: "Orders table and migration" }],
      totals: { files: 38, added: 1240, removed: 610 },
      verdict: { tone: "crit", level: "medium", head: "Medium risk.", text: "It changes how every cart total is computed (total(), line_tax()); 2 changed functions have no test." },
      blast: {
        rings: [
          { n: 6, label: "functions changed", names: ["total()", "line_tax()", "Order.create()", "to_cents()", "receipt_total()", "export_rows()"], code: true },
          { n: 23, label: "callers", names: ["receipt", "ledger export", "coupons", "admin report", "order email", "checkout"] },
          { n: 4, label: "public names", names: ["POST /checkout", "web/checkout.html", "web/receipt.html", "ledger.csv"], code: true },
        ],
        exceptions: [
          { tone: "crit", text: "Untested: line_tax(), Order.create()", sub: "test_receipt_total, test_ledger_export fail on the branch" },
          { tone: "warn", text: "freeship also changes cart.py", sub: "Merging the two conflicts: merge checkout-v2 first, then send freeship back to rebase" },
          { tone: "ctx", text: "Replaces “Round the order total once, at the end.”", sub: "Tax per line in integer cents, rounded half up.", record: "d-41", confirm: true },
        ],
        tests: "214 pass · 2 fail on wb/checkout-v2: test_receipt_total, test_ledger_export · merges cleanly into main",
        risk: { level: "medium" },
      },
      checks: { tests, merge: { onto: "main", clean: true, conflicts: [], line: "merges cleanly into main" }, uncommitted: [] },
      pieces: [
        { title: "Totals per line on the server", words: "total(), line_tax(), Order.create() changed; 17 callers, line_tax(), Order.create() untested.", files: ["cart.py", "money.py", "orders.py"], added: 182, removed: 64, risk: "high", hunk: { path: "money.py", where: "money.py · to_cents()", lines: HUNK_CENTS }, hunks: 6, all: [{ path: "money.py", hunks: [{ new_start: 5, lines: HUNK_CENTS }] }, { path: "cart.py", hunks: [{ new_start: 24, lines: HUNK_TOTAL }, { new_start: 40, lines: HUNK_LINE }] }], truncated: false },
        { title: "The checkout page posts the order", words: "web/checkout.js changed. Another agent changes this file too.", files: ["web/checkout.js", "web/checkout.html", "serve.py", "routes.py"], added: 240, removed: 120, risk: "medium", hunk: { path: "serve.py", where: "serve.py · post_checkout()", lines: [L(" ", "@app.post('/checkout')", 12), L("+", "def post_checkout(req):", 13), L("+", "    order = Order.create(req.json['cart'])", 14)] }, hunks: 9, all: [], truncated: false },
        { title: "Orders table and migration", words: "Order.create() changed; 3 callers, Order.create() untested.", files: ["orders.py", "migrations/004_orders.sql"], added: 96, removed: 0, risk: "medium", hunk: { path: "orders.py", where: "orders.py · Order.create()", lines: [L("+", "class Order:", 1), L("+", "    @classmethod", 2), L("+", "    def create(cls, cart):", 3)] }, hunks: 2, all: [], truncated: false },
        { title: "Money rounds half up with Decimal", words: "to_cents() changed; 23 callers, all tested.", files: ["money.py", "coupons.py", "ledger.py", "admin.py", "receipt.py"], added: 88, removed: 61, risk: "low", hunk: { path: "money.py", where: "money.py · to_cents()", lines: HUNK_CENTS }, hunks: 5, all: [], truncated: false },
        { title: "Tests", words: "14 tests added or changed in 9 files.", files: ["tests/test_cart.py", "tests/test_money.py"], added: 324, removed: 75, risk: "low", hunk: { path: "tests/test_cart.py", where: "tests/test_cart.py", lines: [L("+", "def test_line_tax_rounds_half_up():", 88), L("+", "    assert line_tax(Line(price=0.05, qty=1)) == 1", 89)] }, hunks: 14, all: [], truncated: false },
      ],
      more_pieces: 0,
      mechanical: { files: ["package-lock.json", "web/app.js"], added: 310, removed: 290, what: "1 import-only hunk, 1 lockfile" },
    };
  }
  return { ...base, title: name === "freeship" ? "Free shipping over 50 euros" : "Money as integer cents", said: "", turns: name === "freeship" ? 9 : 22,
    commits: [{ sha: "7b2e0c14", subject: name === "freeship" ? "Free shipping over 50 euros" : "Money as integer cents" }],
    totals: { files: name === "freeship" ? 4 : 11, added: name === "freeship" ? 86 : 212, removed: name === "freeship" ? 12 : 148 },
    verdict: { tone: "good", level: "low", head: "Low risk.", text: name === "freeship" ? "Free shipping over 50 euros (shipping(), Cart.total()); every changed function is tested, 9 callers." : "Money as integer cents (to_cents()); every changed function is tested, 23 callers." },
    blast: { rings: [{ n: 2, names: ["shipping()", "Cart.total()"], code: true }, { n: 9, names: ["receipt", "ledger export", "coupons"] }, { n: 1, names: ["web/checkout.html"], code: true }],
      exceptions: [{ tone: "warn", text: "checkout-v2 also changes cart.py", sub: "Merging the two conflicts: merge checkout-v2 first, then send freeship back to rebase" }], tests: `${tests.line} · merges cleanly into main`, risk: { level: "low" } },
    checks: { tests, merge: { onto: "main", clean: true, conflicts: [], line: "merges cleanly into main" }, uncommitted: [] },
    pieces: [{ title: "Free shipping over 50 euros", words: "shipping(), Cart.total() changed; 9 callers, all tested.", files: ["cart.py", "shipping.py"], added: 40, removed: 8, risk: "low", hunk: { path: "cart.py", where: "cart.py · Cart.total()", lines: HUNK_TOTAL }, hunks: 2, all: [], truncated: false },
      { title: "Tests", words: "3 tests added or changed in 1 file.", files: ["tests/test_cart.py"], added: 46, removed: 4, risk: "low", hunk: null, hunks: 1, all: [], truncated: false }],
    more_pieces: 0, mechanical: { files: [], added: 0, removed: 0, what: "" },
  };
}

// ------------------------------------------------------------------ debug
const flags = {}; // agent id -> [turn]
function turnList(id) {
  const a = byId[id];
  const start = ms(a.name === "checkout-v2" ? 300 : a.name === "freeship" ? 40 : 90);
  const count = a.name === "checkout-v2" ? 186 : a.name === "freeship" ? 9 : 22;
  const titles = {
    freeship: ["Explore how shipping is computed", "Added the shipping line", "Free shipping over 50 euros", "Run the cart tests", "Fix the free-shipping threshold", "Show free shipping on the page", "Run all tests", "Commit", "Write the summary"],
  };
  const out = [];
  let t = start;
  for (let i = 1; i <= count; i++) {
    const len = (a.name === "checkout-v2" ? 50 : 120) * 1000 + ((i * 7919) % 60) * 1000;
    const gap = a.name === "checkout-v2" && [41, 88, 130, 160].includes(i) ? 600000 : 15000;
    const r = (i * 31) % 17;
    let kind = r < 6 ? "read" : r < 11 ? "edit" : r < 14 ? "run" : r < 15 ? "fail" : r < 16 ? "you" : "ctx";
    if (a.name === "freeship") kind = ["read", "ctx", "edit", "run", "edit", "edit", "run", "read", "read"][i - 1];
    const title = (titles[a.name] || [])[i - 1] || ["Read the cart code", "Edit totals", "Run the tests", "Fix rounding in receipts", "Read the ledger export", "Update the migration"][i % 6];
    out.push({
      n: i, title, prompt: { text: i === 2 && a.name === "freeship" ? "Go ahead: shipping line first, with tests." : title, by: i === 1 ? "you" : "you" },
      started_ms: t, ended_ms: t + len, over: true, kind, read: 3, edited: kind === "edit" || kind === "ctx" ? ["cart.py", "tests/test_cart.py"] : [], failed: kind === "fail" ? 1 : 0, ran: kind === "run" ? 1 : 0, waited: kind === "you",
      tokens: 12000 + i * (a.name === "checkout-v2" ? 260 : 7000), injected: i % 3 === 0 ? 0 : 2, missed: a.name === "freeship" && i === 2 ? 1 : 0, tests_broke: a.name === "freeship" && i === 2 ? ["test_receipt_total", "test_total_shown"] : [],
      bad: false, flagged: (flags[id] || []).includes(i),
    });
    t += len + gap;
  }
  return out;
}

function turnsOf(id, url) {
  const a = byId[id];
  if (!a) return [404, { error: `no agent ${id}`, error_code: "not_found" }];
  if (empty(url)) return { agent: a, turns: [], phases: [], standout: [], bad: null, bad_from: "", flagged: [], breaks: [], memory: { counts: {}, total: 0, top: null, missed: null, missed_turn: null, receipts: 0 }, context: { tokens: null, window: 200000, pct: null }, missing: [] };
  const turns = turnList(id);
  const fl = flags[id] || [];
  let bad = fl.length ? fl[fl.length - 1] : a.name === "freeship" ? 2 : null;
  const bad_from = fl.length ? "you" : bad ? "tests" : "";
  for (const t of turns) t.bad = t.n === bad;
  const big = a.name === "checkout-v2";
  const phases = big ? [{ name: "Reading", from: 1, to: 40 }, { name: "money.py", from: 41, to: 87 }, { name: "cart.py", from: 88, to: 129 }, { name: "orders.py", from: 130, to: 159 }, { name: "test_cart.py", from: 160, to: 186 }]
    : a.name === "freeship" ? [{ name: "cart.py", from: 1, to: 5 }, { name: "checkout.html", from: 6, to: 9 }] : [{ name: "money.py", from: 1, to: 22 }];
  const standout = big ? [
    { n: 12, kind: "ctx", text: "Had a note it was not reminded of: “Round the order total once, at the end.” (52% match; under 60% is left out)" },
    { n: 41, kind: "you", text: "Waited for you" },
    { n: 77, kind: "fail", text: "1 tool call failed: pytest tests/test_money.py" },
    { n: 118, kind: "edit", text: "Edited 6 files: cart.py, money.py, orders.py" },
    { n: 151, kind: "fail", text: "Tests failed after it: test_receipt_total, test_ledger_export" },
    { n: 179, kind: "fail", text: "1 tool call failed: ruff check ." },
  ] : a.name === "freeship" ? [
    { n: 2, kind: "fail", text: bad_from === "you" ? "You flagged it: this went wrong" : "Tests failed after it: test_receipt_total, test_total_shown" },
    { n: 3, kind: "edit", text: "Edited 2 files: cart.py, tests/test_cart.py" },
    { n: 5, kind: "edit", text: "Edited 2 files: shipping.py, cart.py" },
  ] : [];
  if (big && bad == null) bad = null;
  return {
    agent: a, turns, phases, standout, bad, bad_from, flagged: fl,
    breaks: a.name === "freeship" ? [{ test: "test_receipt_total", ts: T(36) }, { test: "test_total_shown", ts: T(36) }] : [],
    memory: { counts: { decision: big ? 31 : 2, preference: big ? 4 : 0, fact: big ? 6 : 1 }, total: big ? 41 : 3,
      top: { id: "d-7", kind: "decision", text: "Money is integer cents everywhere; floats only at the edges.", times: big ? 58 : 4 },
      missed: a.name === "freeship" ? { id: "d-3", kind: "decision", text: "Shipping is not taxed: tax applies to the items only.", score: 0.58, threshold: 0.6, reason: "58% match; notes under 60% are left out" } : null,
      missed_turn: a.name === "freeship" ? 2 : null, receipts: big ? 140 : 9 },
    context: { tokens: big ? 61000 : 82000, window: 200000, pct: big ? 30 : 41 },
    missing: [],
  };
}

function contextOf(id, n) {
  const a = byId[id];
  const turns = turnList(id);
  const t = turns[n - 1];
  if (!a || !t) return [404, { error: `${a ? a.name : id} has no turn ${n} (${turns.length} so far)`, error_code: "not_found" }];
  const fs = a.name === "freeship" && n === 2;
  return {
    agent: a, turn: n, turns: turns.length, title: t.title, prompt: t.prompt, started_ms: t.started_ms, ended_ms: t.ended_ms, over: true,
    files: [{ path: "cart.py", how: "read, edited" }, { path: "tests/test_cart.py", how: "edited" }, { path: "money.py", how: "read" }, { path: "coupons.py", how: "read" }],
    context: { tokens: 82000, window: 200000, pct: 41, memory_tokens: 1180 },
    memory: {
      injected: [
        { id: "d-7", kind: "decision", text: "Money is integer cents everywhere; floats only at the edges.", score: 0.91, decision: "take", source: "prompt" },
        { id: "d-9", kind: "decision", text: "Customers win: everything a customer sees rounds half up.", score: 0.74, decision: "take", source: "prompt" },
      ],
      counts: { decision: 2 },
      missed: fs ? [{ id: "d-3", kind: "decision", text: "Shipping is not taxed: tax applies to the items only.", score: 0.58, threshold: 0.6, reason: "58% match; notes under 60% are left out", decision: "skip: below min_cos" }] : [],
      left_out: [{ id: "f-2", kind: "fact", text: "test_promo_window is a known flaky test.", score: 0.31, threshold: 0.6, reason: "31% match; notes under 60% are left out", decision: "skip: below min_cos" }],
      threshold: { value: 0.6, name: "knowledge.min_cos", what: "Workbench reminds an agent of one of your decisions, rules or facts when it matches the prompt at least this well" },
      lower: { to: 0.55, more: 3, tokens: 41, notes: [{ text: "Shipping is not taxed: tax applies to the items only.", score: 0.58 }, { text: "Coupons apply before tax.", score: 0.57 }, { text: "Prices on the page include tax.", score: 0.56 }], scope: "project" },
    },
    missing: [],
  };
}

function blastOf(id, n) {
  const a = byId[id];
  if (!a) return [404, { error: `no agent ${id}`, error_code: "not_found" }];
  return {
    agent: a, turn: n, verdict: `Turn ${n} changed Cart.total(): 9 callers reached, 2 tests caught it, 0 reached main.`, tone: "crit",
    rows: [
      { n: 2, what: "tests failed after it", names: ["test_receipt_total", "test_total_shown"], tone: "crit" },
      { n: 9, what: "callers reached", names: ["receipt", "ledger export", "coupons", "admin report", "order email", "checkout", "refund", "invoice"], more: 1, tone: "warn" },
      { n: 1, what: "function changed", names: ["Cart.total()"], tone: "", code: true },
      { n: 0, what: "runtime errors blamed on it", names: ["none in its app's log"], tone: "ok" },
      { n: 0, what: "reached main", names: ["wb/freeship is not merged"], tone: "ok" },
    ],
    diff: { files: [{ path: "cart.py", status: "modified", hunks: [{ new_start: 24, lines: [L(" ", "def total(self) -> int:", 24), L("-", "    taxable = self.subtotal()", 25), { ...L("+", "    taxable = self.subtotal() \\", 25), cur: true }, L("+", "              + self.shipping()", 26), L(" ", "    return taxable + round(taxable * TAX)", 27)] }] },
      { path: "tests/test_cart.py", status: "modified", hunks: [{ new_start: 61, lines: [L("+", "def test_total_with_shipping():", 61), L("+", "    assert Cart([Line(4748)], ship=499).total() == 5639", 62)] }] }], commits: ["7b2e0c1"], from: "commits" },
    files: ["cart.py", "tests/test_cart.py"], functions: [{ path: "cart.py", name: "Cart.total()" }], missing: [],
  };
}

function sessions(url) {
  if (empty(url)) return { repo: SHOP, sessions: [], flagged: null };
  return {
    repo: SHOP, flagged: A.freeship.id,
    sessions: [
      { agent: byId[A.receipts.id], turns: 4, broke: [], flagged: false, flags: flags[A.receipts.id] || [] },
      { agent: byId[A.freeship.id], turns: 9, broke: ["test_receipt_total", "test_total_shown"], flagged: true, flags: flags[A.freeship.id] || [] },
      { agent: byId[A.cents.id], turns: 22, broke: [], flagged: false, flags: flags[A.cents.id] || [] },
      { agent: byId[A["checkout-v2"].id], turns: 186, broke: ["test_receipt_total", "test_ledger_export"], flagged: true, flags: flags[A["checkout-v2"].id] || [] },
    ],
  };
}

// ------------------------------------------------------------------ records
let RECORDS = [
  { id: "d-3", kind: "decision", text: "Shipping is not taxed: tax applies to the items only.", ts: T(60 * 26), tag: "", by: "user", by_label: "you", agent: null, branch: null, superseded: false, task_only: false, files: [], pinned: false },
  { id: "d-41", kind: "decision", text: "Tax per line in integer cents, rounded half up.", ts: T(180), tag: "", by: "assistant:checkout-v2", by_label: "checkout-v2", agent: A["checkout-v2"], branch: "wb/checkout-v2", superseded: false, task_only: false, files: ["cart.py"], pinned: false },
  { id: "d-12", kind: "decision", text: "Round the order total once, at the end.", ts: T(60 * 24 * 23), tag: "superseded", by: "user", by_label: "you", agent: null, branch: null, superseded: true, task_only: false, files: [], pinned: false },
  { id: "d-7", kind: "decision", text: "Money is integer cents everywhere; floats only at the edges.", ts: T(60 * 24 * 4), tag: "", by: "user", by_label: "you", agent: null, branch: null, superseded: false, task_only: false, files: ["money.py"], pinned: true },
  { id: "p-2", kind: "preference", text: "Use the staging database for this task only.", ts: T(60 * 7), tag: "task", by: "user", by_label: "you", agent: null, branch: "wb/receipts", superseded: false, task_only: true, files: [], pinned: false },
  { id: "d-52", kind: "preference", text: "Orders are immutable once paid.", ts: T(95), tag: "", by: "assistant:checkout-v2", by_label: "checkout-v2", agent: A["checkout-v2"], branch: "wb/checkout-v2", superseded: false, task_only: false, files: [], pinned: false },
  { id: "f-2", kind: "fact", text: "test_promo_window is a known flaky test.", ts: T(60 * 30), tag: "", by: "workbench", by_label: "Workbench", agent: null, branch: null, superseded: false, task_only: false, files: [], pinned: false },
  { id: "e-4", kind: "error", text: "ModuleNotFoundError: decimal_utils — use Python's decimal module, there is no decimal_utils.", ts: T(140), tag: "", by: "assistant:cents", by_label: "cents", agent: A.cents, branch: "wb/cents", superseded: false, task_only: false, files: ["money.py"], pinned: false },
];
function records(url) {
  if (empty(url)) return { repo: SHOP, records: [], total: 0, superseded: 0, facets: {}, missing: [] };
  const q = url.searchParams;
  const facets = { kind: {}, by: {}, branch: {} };
  for (const r of RECORDS) {
    facets.kind[r.kind] = (facets.kind[r.kind] || 0) + 1;
    facets.by[r.by_label] = (facets.by[r.by_label] || 0) + 1;
    if (r.branch) facets.branch[r.branch] = (facets.branch[r.branch] || 0) + 1;
  }
  const list = RECORDS.filter((r) => (!q.get("id") || r.id === q.get("id")) && (!q.get("kind") || r.kind === q.get("kind")) && (!q.get("by") || r.by_label === q.get("by")) && (!q.get("branch") || r.branch === q.get("branch")) && (q.get("superseded") || q.get("id") ? true : !r.superseded));
  return { repo: SHOP, records: list, total: RECORDS.length, superseded: RECORDS.filter((r) => r.superseded).length, facets, missing: [] };
}
let threshold = 0.6;

export const routes = [
  ["GET", /^\/api\/dev\/goal-agents$/, () => ({ agents: goalAgents })],
  ["GET", /^\/api\/review$/, (_, url) => reviewList(url)],
  ["GET", /^\/api\/agents\/([^/]+)\/review$/, (m) => reviewOne(m[1])],
  ["POST", /^\/api\/review\/confirm$/, (_, __, b) => { if (b.ok) confirmed.add(b.record); else confirmed.delete(b.record); return { ok: true, record: b.record, confirmed: !!b.ok }; }],
  ["POST", /^\/api\/review\/merge$/, (_, __, b) => {
    const done = [];
    for (const id of b.agents || []) {
      const g = gateOf(id);
      const a = byId[id];
      if (g.unconfirmed) return [409, { error: `1 decision replaces an earlier decision: confirm it is still right (or change it) before merging ${a.branch}.`, error_code: "unconfirmed" }];
      if (g.anyway && !(b.anyway || []).includes(a.branch)) return [409, { error: `${g.warn.join(" and ")}: send ${a.name} back, or type the branch name ${a.branch} to merge anyway.`, error_code: "gated" }];
      const rec = { agent: { id, name: a.name }, branch: a.branch, onto: "main", commit: "c0ffee9b1d2e3f40", before: "4f2a9c1e7d3b5a60", how: "merge", where: SHOP, ts: new Date(NOW).toISOString() };
      merges[id] = rec; done.push(rec);
    }
    return { ok: true, merged: done, message: `Merged ${done.map((d) => d.branch).join(", then ")} into main.` };
  }],
  ["POST", /^\/api\/review\/undo$/, (_, __, b) => {
    const m = merges[b.agent];
    if (!m) return [404, { error: "Workbench merged nothing of this agent's", error_code: "not_found" }];
    m.undone = true;
    return { ok: true, onto: "main", at: m.before, message: `main is back at ${m.before.slice(0, 7)}; the branch ${m.branch} is unchanged.` };
  }],
  ["GET", /^\/api\/debug\/sessions$/, (_, url) => sessions(url)],
  ["GET", /^\/api\/agents\/([^/]+)\/turns$/, (m, url) => turnsOf(m[1], url)],
  ["GET", /^\/api\/agents\/([^/]+)\/turns\/(\d+)\/context$/, (m) => contextOf(m[1], Number(m[2]))],
  ["GET", /^\/api\/agents\/([^/]+)\/turns\/(\d+)\/blast$/, (m) => blastOf(m[1], Number(m[2]))],
  ["POST", /^\/api\/agents\/([^/]+)\/turns\/(\d+)\/flag$/, (m, _, b) => {
    const n = Number(m[2]);
    const l = (flags[m[1]] ||= []).filter((x) => x !== n);
    if (b.on !== false) l.push(n);
    flags[m[1]] = l;
    return { ok: true, agent: m[1], flagged: l };
  }],
  ["GET", /^\/api\/records$/, (_, url) => records(url)],
  ["PATCH", /^\/api\/records\/([^/]+)$/, (m, _, b) => {
    const r = RECORDS.find((x) => x.id === m[1]);
    if (!r) return [404, { error: `no record ${m[1]}`, error_code: "not_found" }];
    const done = [];
    if (b.text) { r.superseded = true; RECORDS.unshift({ ...r, id: `d-${RECORDS.length + 90}`, text: b.text, by: "user", by_label: "you", superseded: false, ts: new Date(NOW).toISOString() }); done.push(`Corrected: “${b.text}” replaces it.`); }
    if (b.pin) { r.files = [...new Set([...(r.files || []), b.pin])]; done.push(`Pinned to ${b.pin}: given whenever an agent edits it.`); }
    if (b.rule) done.push("Made a rule: every agent of the project is told it at session start.");
    return { ok: true, id: r.id, done, rule: null };
  }],
  ["DELETE", /^\/api\/records\/([^/]+)$/, (m) => { const r = RECORDS.find((x) => x.id === m[1]); RECORDS = RECORDS.filter((x) => x.id !== m[1]); return { ok: true, id: m[1], text: r && r.text }; }],
  ["GET", /^\/api\/repos\/([^/]+)\/threshold$/, () => ({ repo: SHOP, value: threshold, default: 0.4, name: "knowledge.min_cos" })],
  ["PUT", /^\/api\/repos\/([^/]+)\/threshold$/, (_, __, b) => {
    const was = threshold; threshold = b.value;
    return { ok: true, repo: SHOP, value: threshold, was, undo: { value: was }, scope: "project", file: `${SHOP}/.chloectx/config.json`, text: `Every agent of this project is now reminded of a decision, rule or fact from a ${Math.round(threshold * 100)}% match (was ${Math.round(was * 100)}%), from its next prompt.` };
  }],
  ["POST", /^\/api\/rules$/, (_, __, b) => ({ rule: { id: "r9", text: b.text, repo: b.repo }, missing: [] })],
];
