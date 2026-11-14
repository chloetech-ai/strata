// Hotspots (SPEC.md, "Fifth build: Hotspots") for the demo repo "shop" of
// Health's fixtures (health.mjs): the files and functions the agents keep
// correcting, redoing or breaking, scored from Workbench's own rows
// (file_edits, test_runs, runtime_errors, steers, questions, incidents, git
// reverts), with what ChloeCtx knows of each (◆). Shapes are the real API's
// (src/hotspots.rs).
//
// The Map screen shows the shop's territory when the page asks with
// ?story=hot (the harness's address, seen as the Referer), so the heat layer
// has the same files; everything else falls through to the other fixtures:
//   /dev/levels.html?screen=map&story=hot
//   /dev/levels.html?screen=health&agent=hotspots&story=hot
import { NOW, routes as baseRoutes } from "./base.mjs";
import { routes as mapRoutes } from "./map.mjs";
import { raw as labRaw } from "./previewlab.mjs";
import { shop as A } from "./health.mjs";

const REPO = "/home/user/src/shop";
const DAY = 864e5;
const min = (m) => NOW - m * 60000;
const days = (d) => NOW - d * DAY;
const iso = (t) => new Date(t).toISOString();
const fromHot = (req) => /[?&]story=hot\b/.test(String(req?.headers?.referer || ""));
const json = (res, status, v) => { res.writeHead(status, { "content-type": "application/json" }); res.end(JSON.stringify(v)); };
const agents = Object.values(A);

// ----------------------------------------------------------------- the map
const FILES = [
  ["src/cart/totals.rs", 420], ["src/cart/coupon.rs", 260], ["src/cart/line.rs", 180], ["src/cart/mod.rs", 40],
  ["src/checkout/flow.rs", 380], ["src/checkout/stripe.rs", 300], ["src/checkout/tax.rs", 210], ["src/checkout/address.rs", 160], ["src/checkout/mod.rs", 30],
  ["src/stock/reserve.rs", 240], ["src/stock/mod.rs", 60],
  ["src/catalog/query.rs", 280], ["src/catalog/model.rs", 200],
  ["src/orders/api.rs", 320], ["src/orders/model.rs", 150],
  ["src/money.rs", 140], ["src/main.rs", 120], ["src/error.rs", 110], ["src/lib.rs", 50],
  ["tests/totals.rs", 260], ["tests/coupon.rs", 180], ["tests/stock.rs", 150], ["tests/checkout.rs", 300],
  ["web/src/search/Facets.tsx", 260], ["web/src/search/Results.tsx", 220], ["web/src/search/useSearch.ts", 140],
  ["web/src/cart/Cart.tsx", 200], ["web/src/cart/CartLine.tsx", 160],
  ["web/src/checkout/Checkout.tsx", 280], ["web/src/checkout/PaymentStep.tsx", 240],
  ["web/src/lib/api.ts", 180], ["web/src/lib/money.ts", 90],
  ["importer/stream.py", 310], ["importer/csv.py", 190],
  ["tests/import/test_stream.py", 200], ["tests/import/test_csv.py", 140],
  ["migrations/0007_orders.sql", 60], ["migrations/0008_coupons.sql", 40],
  ["docs/pricing.md", 120], ["README.md", 140], ["Cargo.toml", 60],
];
function build(files) {
  const root = { name: "shop", path: "", kind: "dir", lines: 0, children: [] };
  for (const [p, n] of files) {
    const parts = p.split("/");
    let node = root;
    for (let i = 0; i < parts.length - 1; i++) {
      const sub = parts.slice(0, i + 1).join("/");
      let c = node.children.find((x) => x.path === sub);
      if (!c) { c = { name: parts[i], path: sub, kind: "dir", lines: 0, children: [] }; node.children.push(c); }
      node = c;
    }
    node.children.push({ name: parts.at(-1), path: p, kind: "file", lines: n });
  }
  const sum = (n) => (n.children ? (n.lines = n.children.reduce((s, c) => s + sum(c), 0)) : n.lines);
  sum(root);
  const count = (n) => (n.children ? (n.files = n.children.reduce((s, c) => s + count(c), 0)) : 1);
  count(root);
  const sort = (n) => { if (n.children) { n.children.sort((a, b) => (b.children ? 1 : 0) - (a.children ? 1 : 0) || b.lines - a.lines); n.children.forEach(sort); } };
  sort(root);
  return root;
}
const tree = build(FILES);
const work = [
  { agent: A.cart, files: [{ path: "src/cart/totals.rs", added: 46, removed: 31, status: "modified" }, { path: "tests/totals.rs", added: 18, removed: 2, status: "modified" }, { path: "src/cart/coupon.rs", added: 9, removed: 4, status: "modified" }], reading: ["src/money.rs"] },
  { agent: A.checkout, files: [{ path: "src/checkout/flow.rs", added: 64, removed: 12, status: "modified" }, { path: "src/checkout/tax.rs", added: 20, removed: 26, status: "modified" }, { path: "src/cart/totals.rs", added: 14, removed: 2, status: "modified" }, { path: "web/src/checkout/PaymentStep.tsx", added: 40, removed: 6, status: "modified" }], reading: [] },
  { agent: A["search-ui"], files: [{ path: "web/src/search/Facets.tsx", added: 72, removed: 28, status: "modified" }, { path: "web/src/search/useSearch.ts", added: 11, removed: 3, status: "modified" }], reading: ["src/catalog/query.rs"] },
  { agent: A.catalog, files: [{ path: "importer/stream.py", added: 58, removed: 22, status: "modified" }, { path: "tests/import/test_stream.py", added: 30, removed: 0, status: "modified" }], reading: [] },
];
const rec = (id, kind, text, ts) => ({ id, kind, text, ts: iso(ts) });
const pins = [
  { path: "src/cart/totals.rs", line: 88, record: rec("c41ad2e0:12", "decision", "Money is integer cents; round half-even once, on the line total, before tax.", days(6)), agent: A.cart },
  { path: "src/cart/coupon.rs", line: null, record: rec("c41ad2e0:31", "fact", "Coupons apply before tax in the EU and after tax in the US.", days(2)), agent: A.checkout },
  { path: "src/stock/reserve.rs", line: 41, record: rec("c41ad2e0:40", "decision", "Reserve stock with SELECT … FOR UPDATE SKIP LOCKED, never a table lock.", days(9)), agent: null },
];
const shopMap = { repo: REPO, tree, agents: work, collisions: [{ path: "src/cart/totals.rs", agents: [A.cart.id, A.checkout.id], kind: "both_changed", detail: "cart is changing round_line_total(); checkout moved the tax line into the same file." }], pins, missing: [] };

// ---------------------------------------------------------------- hotspots
const churn = (counts) => counts.map((n, i) => [Math.floor(days(counts.length - 1 - i) / DAY) * DAY, n]);
const r = (kind, n, text, ids) => ({ kind, n, text, ids });
const ctxRec = (id, kind, text, ts, via = "chloectx") => ({ id, kind, text, ts: ts ? iso(ts) : null, via });
// The score's weights (src/hotspots.rs, WEIGHTS): points per counted thing.
const W = { re_edits: 1, agents: 2, reverts: 3, test_fails: 1.5, flaky: 3, errors: 2.5, steers: 3, questions: 1.5, incidents: 2 };
const pts = (x) => Math.round((x.kind === "agents" ? W.agents * (x.n - 1) : (W[x.kind] || 0) * x.n) * 10) / 10;
const levelOf = (s) => (s >= 15 ? "hot" : s >= 8 ? "warm" : "mild");
const hot = (o) => {
  const reasons = o.reasons.map((x) => ({ ...x, points: pts(x) }));
  const score = Math.round(reasons.reduce((s, x) => s + x.points, 0) * 10) / 10;
  return {
  repo: REPO, symbol: null, symbol_kind: null, line: null, pinned_test: null, routed_model: null, symbols: [], tests: [], records: [],
  last_correction: null, window_days: 14, context: null,
  counts: { edits: 0, re_edits: 0, agents: 0, reverts: 0, test_fails: 0, flaky: 0, errors: 0, steers: 0, questions: 0, incidents: 0, ...o.counts },
  first: iso(o.first_ms), last: iso(o.last_ms), ...o, reasons, score, level: levelOf(score),
  };
};
export const hotspots = [
  hot({
    id: "h2b7c19f0", file: "src/cart/totals.rs", symbol: "round_line_total", symbol_kind: "fn", line: 88,
    counts: { edits: 14, re_edits: 11, agents: 2, reverts: 3, test_fails: 6, steers: 2, questions: 1, incidents: 2 },
    reasons: [
      r("re_edits", 11, "re-edited 11 times by cart and checkout", ["e1", "e4", "e6", "e9"]),
      r("agents", 2, "2 agents edited it: cart, checkout", ["e1", "e9"]),
      r("reverts", 3, "3 reverts: a revert commit and 2 edits put back", ["c9e2a71b", "u1", "u2"]),
      r("test_fails", 6, "tests::totals::applies_coupon failed 6 times after edits here", ["t1", "t2"]),
      r("steers", 2, "you corrected it twice", ["s1", "s2"]),
      r("questions", 1, "1 question to you", ["q7e1a9c04"]),
      r("incidents", 2, "in 2 incidents", ["i4c1f9a20", "i7d2e0b51"]),
    ],
    agents: [A.cart.id, A.checkout.id], first_ms: days(5.2), last_ms: min(5),
    churn: churn([0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 1, 3, 2, 6]),
    symbols: [{ symbol: "round_line_total", kind: "fn", line: 88, edits: 9 }, { symbol: "apply_coupon", kind: "fn", line: 131, edits: 3 }, { symbol: "tax_for", kind: "fn", line: 176, edits: 2 }],
    tests: [{ test: "tests::totals::applies_coupon", runs: 9, failed: 6, flaky: false, command: "cargo test totals::applies_coupon" }, { test: "tests::totals::rounds_half_even", runs: 9, failed: 1, flaky: true, command: "cargo test totals::rounds_half_even" }],
    records: [ctxRec("r5c2e9a10", "rule", "Round half-even, once, on the line total; never per unit.", days(3), "rule"), ctxRec("c41ad2e0:12", "decision", "Money is integer cents; round half-even once, on the line total, before tax.", days(6))],
    suggestion: { kind: "test", text: "Run `cargo test totals::applies_coupon` before you edit, and again after: it failed 6 times after edits here.", test: "tests::totals::applies_coupon", command: "cargo test totals::applies_coupon" },
    summary: "agents re-edited round_line_total 11 times in 5 days (cart 9, checkout 2); 3 reverts: a revert commit and 2 edits put back; tests::totals::applies_coupon failed 6 times after edits here; you corrected it twice; 1 question to you; in 2 incidents; rule ◆ round half-even, once, on the line total; never per unit",
    last_correction: "Stop changing the rounding; ask me if the test still fails.",
  }),
  hot({
    id: "h5d0e4a21", file: "src/stock/reserve.rs", symbol: "reserve", symbol_kind: "fn", line: 41,
    counts: { edits: 7, re_edits: 5, agents: 1, reverts: 1, test_fails: 3, flaky: 1, errors: 1 },
    reasons: [
      r("re_edits", 5, "re-edited 5 times by cart", ["e20"]),
      r("flaky", 1, "tests::stock::reserve_is_atomic is flaky", ["t20"]),
      r("errors", 1, "1 runtime error: deadlock detected (12 hits)", ["x20"]),
      r("reverts", 1, "1 edit put back", ["u20"]),
    ],
    agents: [A.cart.id], first_ms: days(9), last_ms: min(70),
    churn: churn([1, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 1, 2, 0]),
    symbols: [{ symbol: "reserve", kind: "fn", line: 41, edits: 5 }, { symbol: "release", kind: "fn", line: 102, edits: 2 }],
    tests: [{ test: "tests::stock::reserve_is_atomic", runs: 14, failed: 3, flaky: true, command: "cargo test stock::reserve_is_atomic" }],
    records: [ctxRec("c41ad2e0:40", "decision", "Reserve stock with SELECT … FOR UPDATE SKIP LOCKED, never a table lock.", days(9))],
    suggestion: { kind: "model", text: "Route work here to opus: agents on sonnet re-edited it 5 times; the 2 tasks on opus that touched it finished.", model: "opus", from: "sonnet" },
    summary: "agents re-edited reserve 5 times in 9 days; 1 edit put back; test tests::stock::reserve_is_atomic flaky; 1 runtime error (deadlock detected); decision ◆ reserve stock with SELECT … FOR UPDATE SKIP LOCKED",
  }),
  hot({
    id: "h8a1f3c52", file: "web/src/search/Facets.tsx", symbol: "PriceRange", symbol_kind: "function", line: 64,
    counts: { edits: 8, re_edits: 6, agents: 1, test_fails: 2, flaky: 1, steers: 2 },
    reasons: [
      r("re_edits", 6, "re-edited 6 times by search-ui", ["e30"]),
      r("steers", 2, "you corrected it twice", ["s30", "s31"]),
      r("flaky", 1, "Facets.test.tsx › price range keeps its bounds is flaky", ["t30"]),
    ],
    agents: [A["search-ui"].id], first_ms: days(2), last_ms: min(16),
    churn: churn([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 5]),
    symbols: [{ symbol: "PriceRange", kind: "function", line: 64, edits: 6 }, { symbol: "Facets", kind: "function", line: 12, edits: 2 }],
    tests: [{ test: "web/src/search/Facets.test.tsx › price range keeps its bounds", runs: 16, failed: 3, flaky: true, command: "npx jest web/src/search/Facets.test.tsx -t 'price range'" }],
    records: [],
    suggestion: { kind: "rule", text: "No rule or decision names this file, and you corrected it twice. Make what you told search-ui a rule:", rule: "Price range bounds stay as the user set them when results refresh." },
    summary: "agents re-edited PriceRange 6 times in 2 days; you corrected it twice; Facets.test.tsx › price range keeps its bounds is flaky",
    last_correction: "Keep the bounds the user set when the results refresh.",
  }),
  hot({
    id: "h3c9b7d84", file: "importer/stream.py", symbol: "read_batches", symbol_kind: "def", line: 22,
    counts: { edits: 6, re_edits: 4, agents: 1, test_fails: 4, errors: 2, incidents: 1 },
    reasons: [
      r("test_fails", 4, "test_stream.py::test_bad_row_is_reported failed 4 times after edits here", ["t40"]),
      r("re_edits", 4, "re-edited 4 times by catalog", ["e40"]),
      r("errors", 2, "2 runtime errors: UnicodeDecodeError", ["x40", "x41"]),
      r("incidents", 1, "in 1 incident", ["i9e8d7c66"]),
    ],
    agents: [A.catalog.id], first_ms: min(60 * 30), last_ms: min(22),
    churn: churn([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 4]),
    tests: [{ test: "tests/import/test_stream.py::test_bad_row_is_reported", runs: 13, failed: 4, flaky: false, command: "pytest tests/import/test_stream.py -k bad_row" }],
    suggestion: { kind: "test", text: "Run `pytest tests/import/test_stream.py -k bad_row` first: it failed 4 times after edits here.", test: "tests/import/test_stream.py::test_bad_row_is_reported", command: "pytest tests/import/test_stream.py -k bad_row" },
    summary: "agents re-edited read_batches 4 times in 1 day; test_bad_row_is_reported failed 4 times after edits here; 2 runtime errors",
  }),
  hot({
    id: "h6e2a0b17", file: "src/checkout/tax.rs",
    counts: { edits: 5, re_edits: 3, agents: 2, questions: 1 },
    reasons: [r("agents", 2, "2 agents edited it: checkout, cart", ["e50"]), r("re_edits", 3, "re-edited 3 times", ["e51"]), r("questions", 1, "1 question to you", ["q50"])],
    agents: [A.checkout.id, A.cart.id], first_ms: days(3), last_ms: min(40),
    churn: churn([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 2, 2]),
    records: [ctxRec("c41ad2e0:31", "fact", "Coupons apply before tax in the EU and after tax in the US.", days(2))],
    suggestion: null,
    summary: "2 agents edited it 5 times in 3 days; 1 question to you; fact ◆ coupons apply before tax in the EU",
  }),
  hot({
    id: "h1f7c3e66", file: "web/src/lib/money.ts",
    counts: { edits: 4, re_edits: 2, agents: 2, reverts: 1 },
    reasons: [r("reverts", 1, "1 edit put back", ["u60"]), r("re_edits", 2, "re-edited twice", ["e60"]), r("agents", 2, "2 agents edited it: checkout, search-ui", ["e60"])],
    agents: [A.checkout.id, A["search-ui"].id], first_ms: days(4), last_ms: days(1),
    churn: churn([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 1, 1, 0]),
    suggestion: null,
    summary: "agents edited it 4 times in 4 days; 1 edit put back",
  }),
  hot({
    id: "h4b8d2f90", file: "src/checkout/stripe.rs",
    counts: { edits: 2, re_edits: 1, agents: 1, errors: 1 },
    reasons: [r("errors", 1, "1 runtime error: 502 from the Stripe test API", ["x70"]), r("re_edits", 1, "re-edited once", ["e70"])],
    agents: [A.checkout.id], first_ms: days(1), last_ms: min(90),
    churn: churn([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1]),
    suggestion: null, summary: "1 runtime error after edits here",
  }),
].sort((a, b) => b.score - a.score);
// What the next agent to edit the hottest file is told (src/hotspots.rs, context_of).
hotspots[0].context = [
  "Workbench: src/cart/totals.rs is a hotspot of this repository (score 42.5 in 14 days): agents keep correcting it.",
  "- agents re-edited round_line_total 11 times in 5 days (cart 9, checkout 2)",
  "- 3 reverts: a revert commit and 2 edits put back",
  "- tests::totals::applies_coupon failed 6 times after edits here",
  "- 1 question to you",
  "- in 2 incidents",
  "- the user corrected work here: \"Stop changing the rounding; ask me if the test still fails.\"",
  "- ◆ rule: Round half-even, once, on the line total; never per unit.",
  "- ◆ decision (c41ad2e0:12): Money is integer cents; round half-even once, on the line total, before tax.",
  "Before you edit: run `cargo test totals::applies_coupon` first, and again after. If your change goes against a rule or decision above, ask the user (ask_human) instead of changing it back and forth.",
].join("\n");

// The evidence of a hotspot: a timeline, oldest first.
const ev = (id, kind, m, agent, text, o = {}) => ({ id, kind, ts_ms: min(m), ts: iso(min(m)), agent: agent ? agent.id : null, text, detail: null, link: null, ...o });
const EVIDENCE = {
  h2b7c19f0: [
    ev("r1", "record", 60 * 24 * 6, null, "Money is integer cents; round half-even once, on the line total, before tax.", { detail: "ChloeCtx decision c41ad2e0:12", link: { record: "c41ad2e0:12" } }),
    ev("c9e2a71b", "revert", 60 * 24 * 3.1, A.cart, "Revert “round coupons per unit before the discount”", { detail: "commit c9e2a71b on wb/cart", link: { commit: "c9e2a71b" } }),
    ev("q7e1a9c04", "question", 60 * 24 * 3, A.cart, "Round coupon lines half-even or half-up?", { detail: "you answered “Half-even, once on the line” and made it a rule", link: { question: "q7e1a9c04" } }),
    ev("e1", "edit", 46, A.cart, "Edit round_line_total", { detail: "floor(cents * rate) → round_half_up(cents * rate)", link: { story: { agent: A.cart.id, at_ms: min(46) } } }),
    ev("t1", "test_fail", 44, A.cart, "tests::totals::applies_coupon failed", { detail: "left: 1799, right: 1800 · 2 min after the edit", link: { test: "tests::totals::applies_coupon" } }),
    ev("s1", "steer", 41, null, "Don't round per unit: round once, on the line total.", { detail: "you, to cart", link: { story: { agent: A.cart.id, at_ms: min(41) } } }),
    ev("e4", "edit", 38, A.cart, "Edit round_line_total", { detail: "round per line instead of per unit", link: { story: { agent: A.cart.id, at_ms: min(38) } } }),
    ev("u1", "revert", 33, A.cart, "Put back its own edit of round_line_total", { detail: "round_half_up → floor, the text of 5 min before", link: { story: { agent: A.cart.id, at_ms: min(33) } } }),
    ev("e6", "edit", 15.5, A.cart, "Edit apply_coupon", { detail: "discount before rounding", link: { story: { agent: A.cart.id, at_ms: min(15.5) } } }),
    ev("t2", "test_fail", 14.2, A.cart, "tests::totals::applies_coupon failed (6th time)", { detail: "cargo test totals · 11 passed, 1 failed", link: { test: "tests::totals::applies_coupon" } }),
    ev("i4c1f9a20", "incident", 14, A.cart, "cart is stuck: tests::totals::applies_coupon failed 6 times in 14 min", { link: { incident: "i4c1f9a20" } }),
    ev("e9", "edit", 10.2, A.checkout, "Edit tax_for (moved from checkout/tax.rs)", { detail: "checkout, while cart works on the same file", link: { story: { agent: A.checkout.id, at_ms: min(10.2) } } }),
    ev("i7d2e0b51", "incident", 6, A.checkout, "cart and checkout both edit src/cart/totals.rs", { link: { incident: "i7d2e0b51" } }),
    ev("u2", "revert", 5.2, A.cart, "Put back its own edit of round_line_total", { detail: "half-up → half-even, the text of 10 min before", link: { story: { agent: A.cart.id, at_ms: min(5.2) } } }),
    ev("s2", "steer", 5, null, "Stop changing the rounding; ask me if the test still fails.", { detail: "you, to cart", link: { story: { agent: A.cart.id, at_ms: min(5) } } }),
  ],
};
function evidenceOf(h) {
  if (EVIDENCE[h.id]) return EVIDENCE[h.id];
  return h.reasons.flatMap((x, i) => x.ids.slice(0, 2).map((id, j) => ev(id, x.kind === "re_edits" ? "edit" : x.kind === "test_fails" || x.kind === "flaky" ? "test_fail" : x.kind === "errors" ? "error" : x.kind === "steers" ? "steer" : x.kind === "reverts" ? "revert" : x.kind.replace(/s$/, ""),
    (NOW - h.last_ms) / 60000 + 30 * (i + j), x.kind === "steers" ? null : agents.find((a) => a.id === h.agents[0]), x.text, { link: x.kind === "re_edits" ? { story: { agent: h.agents[0], at_ms: h.last_ms } } : null }))).sort((a, b) => a.ts_ms - b.ts_ms);
}

function list(url) {
  const limit = Number(url.searchParams.get("limit") || 50);
  const d = Number(url.searchParams.get("days") || 14);
  return {
    repo: REPO, window: { from_ms: NOW - d * DAY, to_ms: NOW, days: d }, updated_ms: NOW - 21000, every_s: 60,
    hotspots: hotspots.slice(0, limit),
    totals: { hotspots: hotspots.length, hot: hotspots.filter((x) => x.level === "hot").length, files: 41, edits: 214, reverts: 5, test_fails: 15, errors: 4, steers: 4 },
    agents, ctx: { available: true, records: 4 }, missing: [],
  };
}

export const routes = [
  ["GET", /^\/api\/hotspots$/, (_, url) => list(url)],
  ["GET", /^\/api\/hotspots\/agents$/, () => ({
    agents: [
      { agent: A.cart.id, name: A.cart.name, hotspot: "h2b7c19f0", file: "src/cart/totals.rs", symbol: "round_line_total", score: 42.5, level: "hot", since_ms: min(5) },
      { agent: A["search-ui"].id, name: A["search-ui"].name, hotspot: "h8a1f3c52", file: "web/src/search/Facets.tsx", symbol: "PriceRange", score: 15, level: "hot", since_ms: min(16) },
      { agent: "a1ed6e12", name: "ledger", hotspot: "h9f0a1b22", file: "src/ledger/batch.rs", symbol: "post_batch", score: 12, level: "warm", since_ms: min(3) },
    ],
  })],
  ["GET", /^\/api\/hotspots\/([^/]+)$/, (m) => {
    const x = hotspots.find((y) => y.id === m[1]);
    if (!x) return [404, { error: "no such hotspot", error_code: "not_found" }];
    return { hotspot: x, evidence: evidenceOf(x), agents, missing: [] };
  }],
  ["POST", /^\/api\/hotspots\/refresh$/, () => ({ ok: true, hotspots: hotspots.length, ms: 38 })],
  ["POST", /^\/api\/hotspots\/([^/]+)\/act$/, (m, _, b) => {
    const x = hotspots.find((y) => y.id === m[1]);
    if (!x) return [404, { error: "no such hotspot", error_code: "not_found" }];
    if (b?.kind === "pin_test") x.pinned_test = b.test || null;
    else if (b?.kind === "route") x.routed_model = b.model || null;
    else if (b?.kind === "clear") { x.pinned_test = null; x.routed_model = null; }
    else return [400, { error: "kind is pin_test, route or clear", error_code: "bad_request" }];
    return { hotspot: x };
  }],
  ["POST", /^\/api\/rules$/, (_, __, b) => [201, { rule: { id: "r" + Math.random().toString(16).slice(2, 10), repo: b?.repo, text: b?.text, active: true, created: iso(Date.now()), created_ms: Date.now() }, missing: [] }]],
];

// The map and the agents of the shop story (raw routes: they need the
// Referer); other stories get their own fixtures' answers.
const mapOf = mapRoutes.find(([meth, re]) => meth === "GET" && re.test("/api/map"))[2];
export const raw = [
  ["GET", /^\/api\/map$/, (m, url, req, res) => json(res, 200, fromHot(req) ? shopMap : mapOf(m, url, null))],
  ["GET", /^\/api\/agents$/, (m, url, req, res) => {
    if (fromHot(req)) return json(res, 200, { agents });
    const own = labRaw.find(([meth, re]) => meth === "GET" && re.test("/api/agents"));
    if (own) return own[2](m, url, req, res);
    return json(res, 200, baseRoutes[0][2](m, url, null));
  }],
];
