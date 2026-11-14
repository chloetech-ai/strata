// Ship's routes (SPEC.md, "Fourth build", use cases 7 and 11): the blast
// radius of an agent's change and the pull request it would open, with the
// capsule of the decisions behind it. A demo repo "shop": checkout-tax
// charges sales tax by shipping address; cart-ui changes the same cart
// total, promo-codes calls it, orders-export reads the order JSON.
//
//   /dev/levels.html?screen=ship&agent=s0c4e7a1
import { NOW } from "./base.mjs";

const T = (minAgo) => new Date(NOW - minAgo * 60000).toISOString();
const ms = (minAgo) => NOW - minAgo * 60000;
const REPO = "/home/user/src/shop";
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/shop-5c0ffee1";
const SESSIONS = "/home/user/.local/share/chloedb-workbench/sessions";

function agent(id, name, o) {
  return {
    id, name, repo: REPO, cwd: `${WT}/${name}`, branch: `wb/${name}`, parent: null, depth: 0, model: "sonnet", pid: 5100,
    exit_code: null, ended: null, running: true, children: [], session_id: `5e55${id.slice(1)}-0000-4000-8000-000000000000`,
    base: "4be19c2a91d07e3f", goal: null, outcome: null, waiting_for: null, ...o,
  };
}
export const shopAgents = [
  agent("s0c4e7a1", "checkout-tax", { task: "Charge sales tax by the shipping address at checkout, from a rates table", state: "idle", state_since: T(2), created: T(52), started_ms: ms(52), narration: "Done: tax per line, rounded half-even; 3 test files pass.", narration_ts: T(2), last_event: "Stop" }),
  agent("s1a2b3c4", "cart-ui", { task: "Show a per-line price breakdown in the cart drawer", state: "working", state_since: T(1), created: T(44), started_ms: ms(44), narration: "Editing the cart drawer's line rows.", narration_ts: T(0.5), last_event: "Edit src/cart/total.ts" }),
  agent("s2b3c4d5", "promo-codes", { task: "Stackable promo codes with a per-order cap", state: "working", state_since: T(3), created: T(38), started_ms: ms(38), narration: "Running the discount tests.", narration_ts: T(0.8), last_event: "Bash npm test -- tests/checkout/discount" }),
  agent("s3c4d5e6", "orders-export", { task: "CSV export of orders for the accounting team", state: "working", state_since: T(4), created: T(30), started_ms: ms(30), narration: "Reading how orders are serialised.", narration_ts: T(1), last_event: "Read src/api/orders.ts" }),
];
const A = Object.fromEntries(shopAgents.map((a) => [a.name, a]));
const ME = A["checkout-tax"];

const fn = (path, name, o) => ({ id: `${path}#${name}`, name, path, kind: "function", lang: "typescript", callers: [], callers_total: 0, tests: [], decisions: [], agents: [], ...o });
const callers = (...list) => list.map(([path, line, text]) => ({ path, line, text }));

const FUNCTIONS = [
  fn("src/cart/total.ts", "cartTotal", { line: 24, status: "changed", added: 18, removed: 6,
    callers: callers(["src/checkout/session.ts", 41, "const total = cartTotal(cart, address);"], ["src/checkout/discount.ts", 17, "const before = cartTotal(cart, null);"], ["src/api/cart.ts", 30, "res.json({ total: cartTotal(cart, req.address) });"], ["src/ui/CartDrawer.tsx", 58, "const t = cartTotal(cart, shipTo);"]), callers_total: 7,
    tests: ["tests/cart/total.test.ts", "e2e/checkout.spec.ts"], decisions: ["e3b1c2d4:212", "77ab01c2:9"], agents: ["s1a2b3c4", "s2b3c4d5"] }),
  fn("src/cart/total.ts", "taxFor", { line: 52, status: "added", added: 31, removed: 0,
    callers: callers(["src/cart/total.ts", 37, "const tax = taxFor(line, rate);"], ["src/checkout/summary.ts", 22, "taxFor(line, rateFor(address))"]), callers_total: 2,
    tests: ["tests/cart/total.test.ts"], decisions: ["e3b1c2d4:212", "9d4e7a10:58"], agents: ["s1a2b3c4"] }),
  fn("src/tax/rates.ts", "rateFor", { line: 18, status: "added", added: 44, removed: 0,
    callers: callers(["src/cart/total.ts", 33, "const rate = rateFor(address);"]), callers_total: 1,
    tests: [], decisions: ["9d4e7a10:58", "4f20aa91:31"] }),
  fn("src/tax/rates.ts", "RateTable", { kind: "class", line: 4, status: "added", added: 13, removed: 0,
    callers: callers(["src/tax/rates.ts", 19, "const table = RateTable.load(db);"]), callers_total: 1, tests: [], decisions: ["9d4e7a10:58"] }),
  fn("src/checkout/session.ts", "createSession", { line: 38, status: "changed", added: 9, removed: 3,
    callers: callers(["src/api/checkout.ts", 12, "const s = await createSession(cart, body.shipping);"], ["src/api/checkout.ts", 48, "createSession(cart, saved.address)"]), callers_total: 3,
    tests: ["tests/checkout/session.test.ts", "e2e/checkout.spec.ts"], decisions: ["4f20aa91:31"], agents: [] }),
  fn("src/api/orders.ts", "toOrderJson", { line: 71, status: "changed", added: 6, removed: 1,
    callers: callers(["src/api/orders.ts", 102, "return orders.map(toOrderJson);"], ["src/api/admin.ts", 64, "body: toOrderJson(order)"]), callers_total: 4,
    tests: ["tests/api/orders.test.ts"], decisions: [], agents: ["s3c4d5e6"] }),
];

const TESTS = [
  { path: "tests/cart/total.test.ts", score: 11, covers: ["src/cart/total.ts#cartTotal", "src/cart/total.ts#taxFor"], why: ["names cartTotal ×6, taxFor ×4", "imports src/cart/total"],
    runs: { total: 14, passed: 14, failed: 0, flaky: false, p50_ms: 420, last_passed: true, last: T(6), names: ["cartTotal adds tax per line", "taxFor rounds half-even", "cartTotal without an address"] } },
  { path: "tests/checkout/session.test.ts", score: 5, covers: ["src/checkout/session.ts#createSession"], why: ["names createSession ×3", "imports src/checkout/session"],
    runs: { total: 11, passed: 9, failed: 2, flaky: true, p50_ms: 1830, last_passed: true, last: T(9), names: ["createSession keeps the shipping address", "createSession times out on a slow rates table"] } },
  { path: "e2e/checkout.spec.ts", score: 4, covers: ["src/cart/total.ts#cartTotal", "src/checkout/session.ts#createSession"], why: ["names cartTotal, createSession"], runs: null },
  { path: "tests/api/orders.test.ts", score: 2, covers: ["src/api/orders.ts#toOrderJson"], why: ["imports src/api/orders"],
    runs: { total: 6, passed: 6, failed: 0, flaky: false, p50_ms: 260, last_passed: true, last: T(14), names: ["order JSON has tax_cents"] } },
];

const DECISIONS = [
  { id: "e3b1c2d4:212", kind: "decision", text: "Tax is computed per line in integer cents and rounded half-even, never on the order total.", ts: T(31), session: ME.session_id, agent: ME,
    files: ["src/cart/total.ts"], via: ["blame", "file"], functions: ["src/cart/total.ts#cartTotal", "src/cart/total.ts#taxFor"], superseded: false, line: { path: "src/cart/total.ts", line: 61 } },
  { id: "9d4e7a10:58", kind: "decision", text: "Tax rates live in the tax_rates table keyed by (country, region); never hard-code a rate.", ts: "2026-11-06T10:12:00Z", session: "9d4e7a10-1111", agent: null,
    files: ["src/tax/rates.ts", "migrations/0019_tax_rates.sql"], via: ["file", "graph"], functions: ["src/tax/rates.ts#rateFor", "src/tax/rates.ts#RateTable", "src/cart/total.ts#taxFor"], superseded: false, line: null },
  { id: "4f20aa91:31", kind: "preference", text: "US prices are shown before tax; EU prices include VAT.", ts: "2026-11-02T16:40:00Z", session: "4f20aa91-2222", agent: null,
    files: ["src/checkout/summary.ts"], via: ["graph"], functions: ["src/tax/rates.ts#rateFor", "src/checkout/session.ts#createSession"], superseded: false, line: null, entity: "tax" },
  { id: "77ab01c2:9", kind: "decision", text: "Round the order total once, at the end.", ts: "2026-10-21T09:05:00Z", session: "77ab01c2-3333", agent: null,
    files: ["src/cart/total.ts"], via: ["file"], functions: ["src/cart/total.ts#cartTotal"], superseded: true, superseded_by: "e3b1c2d4:212", line: null },
];

const BLAST = {
  agent: ME, repo: REPO, base: "4be19c2a91d07e3f", head: "c81f0d3e5a7b9c21", branch: "wb/checkout-tax",
  totals: { files: 7, added: 162, removed: 14, functions: 6, callers: 18, tests: 4, decisions: 4, agents: 3 },
  risk: {
    level: "medium", score: 11,
    line: "Medium risk: 6 functions changed with 18 callers; rateFor and RateTable have no test that names them and cart-ui changes src/cart/total.ts too.",
    reasons: [
      "2 changed functions have no test that names them: rateFor, RateTable",
      "cart-ui also changed src/cart/total.ts (+12 −4)",
      "promo-codes changed src/checkout/discount.ts, which calls cartTotal",
      "tests/checkout/session.test.ts is flaky: passed and failed on one commit",
      "1 decision about these lines was superseded: 77ab01c2:9",
    ],
  },
  files: [
    { path: "migrations/0019_tax_rates.sql", status: "added", added: 22, removed: 0, functions: [] },
    { path: "src/api/orders.ts", status: "modified", added: 6, removed: 1, functions: ["src/api/orders.ts#toOrderJson"] },
    { path: "src/cart/total.ts", status: "modified", added: 49, removed: 6, functions: ["src/cart/total.ts#cartTotal", "src/cart/total.ts#taxFor"] },
    { path: "src/checkout/session.ts", status: "modified", added: 9, removed: 3, functions: ["src/checkout/session.ts#createSession"] },
    { path: "src/tax/rates.ts", status: "added", added: 57, removed: 0, functions: ["src/tax/rates.ts#RateTable", "src/tax/rates.ts#rateFor"] },
    { path: "tests/cart/total.test.ts", status: "modified", added: 15, removed: 4, functions: [] },
    { path: "tests/api/orders.test.ts", status: "modified", added: 4, removed: 0, functions: [] },
  ],
  functions: FUNCTIONS,
  tests: TESTS,
  decisions: DECISIONS,
  related_files: [{ path: "src/checkout/summary.ts", via: ["4f20aa91:31"] }, { path: "src/admin/rates.tsx", via: ["9d4e7a10:58"] }],
  agents: [
    { agent: A["cart-ui"], kinds: ["same_file"], files: ["src/cart/total.ts"], functions: ["src/cart/total.ts#cartTotal", "src/cart/total.ts#taxFor"], detail: "cart-ui also changed src/cart/total.ts (+12 −4)" },
    { agent: A["promo-codes"], kinds: ["calls_changed"], files: ["src/checkout/discount.ts"], functions: ["src/cart/total.ts#cartTotal"], detail: "promo-codes changed src/checkout/discount.ts, which calls cartTotal" },
    { agent: A["orders-export"], kinds: ["reading"], files: ["src/api/orders.ts"], functions: ["src/api/orders.ts#toOrderJson"], detail: "orders-export read src/api/orders.ts 3 min ago" },
  ],
  sources: { test_runs: true, chloectx: true, graph: true, blame: 6 },
  missing: [],
};

const CAP_DIR = `${SESSIONS}/${ME.id}/capsule`;
let capsule = {
  dir: CAP_DIR, title: "checkout-tax: Charge sales tax by the shipping address", records: 186,
  by_kind: { prompt: 14, answer: 14, episode: 31, summary: 6, decision: 5, preference: 1, fact: 3, todo: 1, note: 2, entity: 104, error: 5 },
  bytes: 412331, sha256: "9b1e0c47d2a35f8e6b0d9c2a71f4e3b8a5c6d7e8f9012345678abcdef0123456", created: T(1), since: T(52),
  pack_ms: 840, scope: "the project's memory since checkout-tax started (52 min)",
  mount: `chloectx mount ${CAP_DIR}`, claude: `claude mcp add checkout-tax-capsule -- chloectx mount ${CAP_DIR}`,
};

function prBody() {
  return `## Summary

Charge sales tax by the shipping address at checkout, from a rates table.

- Adds \`src/tax/rates.ts\`: \`RateTable\` and \`rateFor(address)\`, rates read from the new \`tax_rates\` table (migration 0019).
- \`cartTotal\` adds tax per line through \`taxFor\`, in integer cents, rounded half-even.
- \`createSession\` takes the shipping address; \`toOrderJson\` returns \`tax_cents\`.

7 files, +162 −14, on \`wb/checkout-tax\` from \`4be19c2\`.

## Decisions behind it

- ◆ \`e3b1c2d4:212\` decision: Tax is computed per line in integer cents and rounded half-even, never on the order total.
- ◆ \`9d4e7a10:58\` decision: Tax rates live in the tax_rates table keyed by (country, region); never hard-code a rate.
- ◆ \`4f20aa91:31\` preference: US prices are shown before tax; EU prices include VAT.
- ◆ \`77ab01c2:9\` (superseded by \`e3b1c2d4:212\`): Round the order total once, at the end.

## Blast radius

Medium risk: 6 functions changed with 18 callers; rateFor and RateTable have no test that names them and cart-ui changes src/cart/total.ts too.

- Functions: \`cartTotal\`, \`taxFor\` (new), \`rateFor\` (new), \`RateTable\` (new), \`createSession\`, \`toOrderJson\`
- Tests that cover them: \`tests/cart/total.test.ts\` (14 of 14), \`tests/checkout/session.test.ts\` (9 of 11, flaky), \`e2e/checkout.spec.ts\` (not run), \`tests/api/orders.test.ts\` (6 of 6)
- Other agents: cart-ui also changed src/cart/total.ts (+12 −4); promo-codes changed src/checkout/discount.ts, which calls cartTotal; orders-export read src/api/orders.ts 3 min ago

## Tests run

- \`npm test -- tests/cart\`: passed (14 tests, 0.4 s)
- \`npm test -- tests/checkout\`: 1 failed, then passed
- \`npm test -- tests/api/orders\`: passed (6 tests)

## Ask why

This pull request carries a capsule of the work behind it: 186 records of checkout-tax's session (5 decisions, 14 prompts and their answers), packed by ChloeCtx. Ask it instead of guessing:

\`\`\`
claude mcp add checkout-tax-capsule -- chloectx mount ${CAP_DIR}
\`\`\`

then, in Claude Code, \`capsule_ask\` "why per line and not on the total?". It answers from the records, with no model call.
`;
}

const PR = () => ({
  agent: ME, title: "Charge sales tax by the shipping address at checkout", branch: "wb/checkout-tax", base_branch: "main", base: "4be19c2a91d07e3f",
  body: prBody(), body_file: `${SESSIONS}/${ME.id}/pr.md`,
  gh: `gh pr create --base main --head wb/checkout-tax --title "Charge sales tax by the shipping address at checkout" --body-file ${SESSIONS}/${ME.id}/pr.md`,
  push: "git push -u origin wb/checkout-tax",
  sections: {
    summary: ["Adds src/tax/rates.ts: RateTable and rateFor(address)", "cartTotal adds tax per line through taxFor, rounded half-even", "createSession takes the shipping address; toOrderJson returns tax_cents"],
    commits: [{ sha: "c81f0d3e", subject: "Tax per line, rounded half-even" }, { sha: "a17e2b90", subject: "Rates table and rateFor" }, { sha: "5d03c4aa", subject: "tax_cents in the order JSON" }],
    decisions: DECISIONS.map(({ id, kind, text, ts, superseded, superseded_by }) => ({ id, kind, text, ts, superseded, superseded_by: superseded_by || null })),
    blast: { level: "medium", line: BLAST.risk.line, functions: 6, tests: 4, agents: ["cart-ui", "promo-codes", "orders-export"] },
    tests: [
      { command: "npm test -- tests/cart", ok: true, summary: "14 passed", ts: T(6) },
      { command: "npm test -- tests/checkout", ok: false, summary: "1 failed: createSession times out on a slow rates table", ts: T(11) },
      { command: "npm test -- tests/checkout", ok: true, summary: "11 passed", ts: T(9) },
      { command: "npm test -- tests/api/orders", ok: true, summary: "6 passed", ts: T(14) },
    ],
  },
  capsule,
  missing: [],
});

const ANSWER = {
  via: "capsule", ms: 41, ids: ["e3b1c2d4:212", "e3b1c2d4:198", "77ab01c2:9"],
  text: `Rules and decisions:
- [e3b1c2d4:212] decision: Tax is computed per line in integer cents and rounded half-even, never on the order total.
- [77ab01c2:9] decision (superseded by e3b1c2d4:212): Round the order total once, at the end.

Closest exchange:
- [e3b1c2d4:198] prompt (you): "A €9.99 × 3 cart came out a cent off against the invoice. The invoice rounds each line."
  answer: Rounding once at the end disagrees with invoices that tax each line; per-line half-even matches them and the accounting export.`,
  agents: { "e3b1c2d4:212": ME, "e3b1c2d4:198": ME },
  missing: [],
};

export const routes = [
  ["GET", /^\/api\/agents\/([^/]+)\/blast$/, (m) => (m[1] === ME.id ? BLAST : [404, { error: `no agent ${m[1]}`, error_code: "not_found" }])],
  ["GET", /^\/api\/agents\/([^/]+)\/pr$/, (m) => (m[1] === ME.id ? PR() : [404, { error: `no agent ${m[1]}`, error_code: "not_found" }])],
  ["GET", /^\/api\/agents\/([^/]+)\/capsule$/, () => ({ available: true, capsule })],
  ["POST", /^\/api\/agents\/([^/]+)\/capsule$/, async () => {
    await new Promise((r) => setTimeout(r, 900));
    capsule = { ...capsule, created: new Date().toISOString() };
    return { available: true, capsule };
  }],
  ["POST", /^\/api\/agents\/([^/]+)\/capsule\/ask$/, async (_, __, body) => {
    await new Promise((r) => setTimeout(r, 300));
    return { ...ANSWER, question: body && body.question };
  }],
];
