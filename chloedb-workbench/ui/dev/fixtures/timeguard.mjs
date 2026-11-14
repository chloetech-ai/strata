// Time and guard (SPEC.md, "Fourth build", rows 8, 9 and 6) for a demo
// repo, "shop": you set the goal "Checkout v2" at 6:48 and a lead started
// four agents. The routes answer as the real ones do:
//
//   GET  /api/time?repo=          the timeline: snapshots kept, events per minute
//   GET  /api/time?at=&repo=      Mission's and Map's data as of then
//   GET  /api/guarded?repo=       the repo's guarded list; PUT replaces it
//   GET  /api/changes?repo=       change requests; POST /api/changes proposes one
//   POST /api/changes/:id/approve, /decline
//   GET  /api/route?repo=&task=   which model similar past tasks did best on
//
// The story's clock is local time: the mock and the browser are run in one
// time zone (TZ=Asia/Dubai and timezoneId for the screenshots), and "7:12"
// is the last 7:12 at least 40 minutes ago.
export const SHOP = "/home/user/src/shop";
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/shop-5c0ffee1";
const NOW = Date.now();
const at712 = (() => {
  const d = new Date(NOW);
  d.setHours(7, 12, 0, 0);
  if (d.getTime() > NOW - 40 * 60000) d.setDate(d.getDate() - 1);
  return d.getTime();
})();
/** A time `min` minutes after 7:12 (negative: before). */
const A = (min) => at712 + min * 60000;
const iso = (ms) => new Date(ms).toISOString();
const hex8 = () => Math.floor(Math.random() * 0xffffffff).toString(16).padStart(8, "0");

function agent(id, name, o) {
  return {
    id, name, repo: SHOP, cwd: `${WT}/${name}`, branch: `wb/${name}`, parent: "b0000001", depth: 1, model: "sonnet",
    pid: 5100, exit_code: null, ended: null, running: true, children: [], base: "4e1d0c9a7b21", goal: "g5c0ffee1",
    session_id: `c4ec${id.slice(1)}-0000-4000-8000-000000000000`, waiting_for: null, preview_url: null,
    ...o, created: iso(o.created), state_since: iso(o.state_since), narration_ts: iso(o.narration_ts), started_ms: o.created,
  };
}
// The fleet as it was at 7:12.
const then = [
  agent("b0000001", "lead", { parent: null, depth: 0, model: "opus", task: "Checkout v2: saved carts, Apple Pay and one-click reorder", state: "working", state_since: A(-3), created: A(-24), outcome: null, narration: "Waiting on cart and payments before starting reorder.", narration_ts: A(-1), last_event: "wait_agent cart", children: ["b1ca7701", "b2fa9e02", "b3e8b003", "b4d1f004"] }),
  agent("b1ca7701", "cart", { task: "Saved carts survive logout and device changes", state: "working", state_since: A(-2), created: A(-20), outcome: "o5c000001", narration: "Moving the cart from localStorage to the carts table; 6 of 9 tests pass.", narration_ts: A(-0.5), last_event: "Bash npm test cart" }),
  agent("b2fa9e02", "payments", { task: "Apple Pay at checkout through Stripe's Payment Request button", state: "working", state_since: A(-6), created: A(-20), outcome: "o5c000002", narration: "Registering the Apple Pay domain file in public/.well-known.", narration_ts: A(-1.5), last_event: "Write public/.well-known/apple-developer-merchantid-domain-association" }),
  agent("b3e8b003", "web", { task: "Checkout page shows the saved cart and the Apple Pay button", state: "working", state_since: A(-4), created: A(-14), outcome: "o5c000004", narration: "Reading how CartSummary gets its items before wiring the saved cart.", narration_ts: A(-0.8), last_event: "Read src/cart/store.ts" }),
  agent("b4d1f004", "migrate", { model: "haiku", task: "Migration for saved carts: carts.saved_at and an index on (user_id, saved_at)", state: "waiting", state_since: A(-3), created: A(-16), outcome: "o5c000003", narration: "Proposed: npx prisma migrate deploy (guarded: migrations).", narration_ts: A(-3), waiting_for: "Change request: npx prisma migrate deploy", last_event: "propose_change" }),
];
const byName = Object.fromEntries(then.map((a) => [a.name, a]));
// The fleet now: two outcomes done since, reorder started.
const now = then.map((a) => ({ ...a }));
Object.assign(now.find((a) => a.name === "cart"), { state: "exited", running: false, exit_code: 0, ended: iso(A(31)), state_since: iso(A(31)), narration: "Saved carts are in the carts table; 9 of 9 tests pass." });
Object.assign(now.find((a) => a.name === "migrate"), { state: "exited", running: false, exit_code: 0, ended: iso(A(9)), state_since: iso(A(9)), waiting_for: null, narration: "The migration ran on staging: carts.saved_at is there." });
now.push(agent("b5a0de05", "reorder", { task: "One-click reorder from the order history", state: "working", state_since: NOW - 120000, created: A(33), outcome: "o5c000005", narration: "Adding a Reorder button to OrderRow.", narration_ts: NOW - 60000, last_event: "Edit src/orders/OrderRow.tsx" }));

const goal = { id: "g5c0ffee1", repo: SHOP, text: "Checkout v2: saved carts, Apple Pay and one-click reorder", state: "active", lead: "b0000001", created: iso(A(-24)), done: null };
const oc = (n, title, detail, state, agent, progress, o = {}) => ({
  id: `o5c00000${n}`, goal: goal.id, title, detail, state, agent: agent ? byName[agent].id : null, agent_name: agent, agent_state: agent ? byName[agent].state : null,
  ord: n, after: null, progress, created: iso(A(-22)), updated: iso(A(-2)), updated_ms: A(-2), ...o,
});
const outcomesThen = [
  oc(1, "Saved carts survive logout", "A cart saved on one device is there after logging in on another.", "active", "cart", 55),
  oc(2, "Apple Pay at checkout", "The Payment Request button pays with Apple Pay in Safari; a test charge succeeds.", "active", "payments", 30),
  oc(3, "Migration: carts.saved_at", "The column and its index exist on staging; the migration is reversible.", "needs_you", "migrate", 80),
  oc(4, "Checkout shows the saved cart", "CartSummary reads the saved cart; the Apple Pay button shows when available.", "active", "web", 15),
  oc(5, "One-click reorder", "Reorder on a past order fills the cart with what is still in stock.", "next", null, 0, { after: "o5c000001" }),
  oc(6, "Docs and changelog", "Starts when the three features land.", "next", null, 0, { after: "o5c000005" }),
];
const total = (os) => {
  const live = os.filter((o) => o.state !== "dropped");
  return { outcomes_total: live.length, outcomes_done: live.filter((o) => o.state === "done").length, needs_you: live.filter((o) => o.state === "needs_you").length, progress: Math.round(live.reduce((n, o) => n + o.progress, 0) / live.length) };
};
const rec = (id, kind, text, ms, a, files = []) => ({ id, kind, text, ts: iso(ms), ts_ms: ms, session: a ? a.session_id : null, agent: a || null, files });
const learnedThen = [
  rec("c4ec0001:41", "decision", "Saved carts live in the carts table, keyed by user, never in localStorage.", A(-18), byName.lead),
  rec("c4ec7701:12", "fact", "Prisma's migrate deploy applies pending migrations only; it never resets the database.", A(-9), byName.migrate),
  rec("c4ec9e02:7", "decision", "Apple Pay goes through Stripe's Payment Request button, not Apple's JS directly.", A(-7), byName.payments),
  rec("c4ec7701:19", "fix", "“P3009 migrate found failed migrations” → mark it rolled back with prisma migrate resolve.", A(-4), byName.migrate),
];
const pulse = (end) => Array.from({ length: 60 }, (_, i) => {
  const t = Math.floor(end / 60000) * 60000 - (59 - i) * 60000;
  const m = (t - A(-24)) / 60000;
  return [t, m < 0 ? 0 : Math.max(0, Math.round(8 + 5 * Math.sin(m * 0.5) + 3 * Math.cos(m * 1.7) + (m > 10 ? 4 : 0)))];
});

// The map of then: the tree of the main checkout, each agent's diff then.
const FILES = [
  ["src/cart/store.ts", 312], ["src/cart/sync.ts", 188], ["src/cart/types.ts", 74], ["src/cart/CartSummary.tsx", 240], ["src/cart/index.ts", 20],
  ["src/checkout/Checkout.tsx", 380], ["src/checkout/PaymentRequest.tsx", 164], ["src/checkout/address.ts", 140], ["src/checkout/totals.ts", 118],
  ["src/orders/OrderHistory.tsx", 260], ["src/orders/OrderRow.tsx", 150], ["src/orders/api.ts", 120],
  ["src/payments/stripe.ts", 220], ["src/payments/webhooks.ts", 280], ["src/payments/refunds.ts", 160],
  ["src/catalog/Product.tsx", 300], ["src/catalog/search.ts", 210], ["src/catalog/stock.ts", 90],
  ["src/auth/session.ts", 180], ["src/auth/login.tsx", 140], ["src/lib/http.ts", 96], ["src/lib/money.ts", 70],
  ["prisma/schema.prisma", 240], ["prisma/migrations/0041_orders_index/migration.sql", 18],
  ["tests/cart/store.test.ts", 220], ["tests/cart/sync.test.ts", 140], ["tests/checkout/payment.test.ts", 180], ["tests/orders/history.test.ts", 120],
  ["scripts/deploy.sh", 64], ["scripts/seed.ts", 110], ["public/index.html", 40], ["package.json", 58], ["README.md", 120],
];
function tree() {
  const root = { name: "shop", path: "", kind: "dir", lines: 0, files: 0, children: [] };
  for (const [p, n] of FILES) {
    const parts = p.split("/");
    let node = root;
    for (let i = 0; i < parts.length - 1; i++) {
      const sub = parts.slice(0, i + 1).join("/");
      let c = node.children.find((x) => x.path === sub);
      if (!c) { c = { name: parts[i], path: sub, kind: "dir", lines: 0, files: 0, children: [] }; node.children.push(c); }
      node = c;
    }
    node.children.push({ name: parts.at(-1), path: p, kind: "file", lines: n });
  }
  const sum = (n) => { if (n.kind !== "dir") return [n.lines, 1]; let l = 0, f = 0; for (const c of n.children) { const [a, b] = sum(c); l += a; f += b; } n.lines = l; n.files = f; return [l, f]; };
  sum(root);
  const sort = (n) => { if (n.children) { n.children.sort((a, b) => (b.kind === "dir") - (a.kind === "dir") || b.lines - a.lines); n.children.forEach(sort); } };
  sort(root);
  return root;
}
const f = (path, added, removed, status = "modified") => ({ path, added, removed, status });
const mapThen = () => ({
  repo: SHOP, tree: tree(),
  agents: [
    { agent: byName.cart, files: [f("src/cart/store.ts", 64, 22), f("src/cart/sync.ts", 48, 6), f("tests/cart/store.test.ts", 40, 4)], reading: ["src/auth/session.ts"] },
    { agent: byName.payments, files: [f("src/checkout/PaymentRequest.tsx", 52, 8), f("src/payments/stripe.ts", 18, 3)], reading: ["src/checkout/totals.ts"] },
    { agent: byName.web, files: [f("src/cart/CartSummary.tsx", 12, 4), f("src/cart/store.ts", 6, 1)], reading: ["src/cart/store.ts", "src/checkout/Checkout.tsx"] },
    { agent: byName.migrate, files: [f("prisma/schema.prisma", 3, 0), f("prisma/migrations/0042_saved_carts/migration.sql", 14, 0, "added")], reading: [] },
  ],
  collisions: [
    { path: "src/cart/store.ts", agents: [byName.cart.id, byName.web.id], kind: "both_changed", detail: "cart and web both changed src/cart/store.ts" },
  ],
  pins: [
    { path: "src/cart/store.ts", line: 41, record: { id: "c4ec0001:41", kind: "decision", text: "Saved carts live in the carts table, keyed by user, never in localStorage.", ts: iso(A(-18)) }, agent: byName.lead },
    { path: "src/checkout/PaymentRequest.tsx", line: 12, record: { id: "c4ec9e02:7", kind: "decision", text: "Apple Pay goes through Stripe's Payment Request button, not Apple's JS directly.", ts: iso(A(-7)) }, agent: byName.payments },
    { path: "src/lib/money.ts", line: 3, record: { id: "c4ec0001:9", kind: "preference", text: "Money is integer cents everywhere; format only at the edge.", ts: iso(A(-60 * 24 * 5)) }, agent: null },
  ],
  missing: [],
});

// Snapshots: every 10 minutes since 6:40 and at each goal change; they are
// kept a day. Events per minute from the story's start to now.
const snapshots = [];
for (let t = A(-32); t <= NOW; t += 600000) snapshots.push({ id: `t${Math.round(t / 1000)}`, ts_ms: t, reason: "timer", detail: null });
snapshots.push({ id: `t${Math.round(A(-24) / 1000)}`, ts_ms: A(-24), reason: "goal", detail: "goal set: Checkout v2" });
snapshots.push({ id: `t${Math.round(A(-2.5) / 1000)}`, ts_ms: A(-2.5), reason: "goal", detail: "outcome needs you: Migration: carts.saved_at" });
snapshots.push({ id: `t${Math.round(A(31) / 1000)}`, ts_ms: A(31), reason: "goal", detail: "outcome done: Saved carts survive logout" });
snapshots.sort((a, b) => a.ts_ms - b.ts_ms);
const events = [];
for (let t = Math.floor(A(-40) / 60000) * 60000; t <= NOW; t += 60000) {
  const m = (t - A(-24)) / 60000;
  events.push([t, m < -6 ? 0 : Math.max(0, Math.round(6 + 5 * Math.sin(m * 0.37) + 4 * Math.cos(m * 1.3) + (m > 8 && m < 70 ? 6 : 0)))]);
}

// As the server answers it: Mission's goal detail by goal id too.
function asOf(at) {
  const v = asOfDetail(at);
  v.mission.details = { [v.mission.detail.goal.id]: v.mission.detail };
  return v;
}

function asOfDetail(at) {
  const snap = snapshots.reduce((b, s) => (Math.abs(s.ts_ms - at) < Math.abs(b.ts_ms - at) ? s : b), snapshots[0]);
  const os = outcomesThen;
  const g = { ...goal, ...total(os) };
  return {
    available: true, at_ms: at, repo: SHOP, snapshot: snap, distance_ms: Math.abs(snap.ts_ms - at),
    heads: [
      { agent: byName.cart.id, name: "cart", branch: "wb/cart", head: "a1b2c3d4e5f6", tree: "a1b2c3d4e5f6", dirty: true, files: 3 },
      { agent: byName.payments.id, name: "payments", branch: "wb/payments", head: "77e0c2f1a9d3", tree: "5d1e88f0c2b7", dirty: true, files: 2 },
      { agent: byName.web.id, name: "web", branch: "wb/web", head: "4e1d0c9a7b21", tree: "9f3e1aa0d2c4", dirty: true, files: 2 },
      { agent: byName.migrate.id, name: "migrate", branch: "wb/migrate", head: "c08d1e2f3a4b", tree: "c08d1e2f3a4b", dirty: false, files: 2 },
    ],
    agents: then,
    mission: {
      goals: [g],
      detail: {
        goal: g, outcomes: os, agents: then, open_questions: 1, learned: learnedThen,
        heads_up: [{ path: "src/cart/store.ts", agents: [byName.cart.id, byName.web.id], kind: "both_changed", detail: "cart and web both changed src/cart/store.ts" }],
        pulse: pulse(at), missing: [],
      },
      questions: [{
        id: "q5c0a0001", agent: byName.payments.id, agent_name: "payments", outcome: "o5c000002", repo: SHOP, kind: "ask",
        text: "Show Apple Pay only in Safari, or also Google Pay where the browser offers it?", context: "Stripe's button offers both; the goal names Apple Pay only.",
        options: [{ label: "Apple Pay only", matches_precedent: false }, { label: "Both, whatever the browser offers", matches_precedent: true }], recommended: 1,
        precedent: null, state: "open", answer: null, answered_by: null, rule: null, created: iso(A(-5)), created_ms: A(-5), answered: null, answered_ms: null,
      }],
      rules: [{ id: "r5c0a0001", repo: SHOP, text: "Money is integer cents; never floats.", question: null, ctx_id: "c4ec0001:9", active: true, created: iso(A(-60 * 24 * 5)), created_ms: A(-60 * 24 * 5) }],
    },
    map: mapThen(),
    missing: [],
  };
}

// Guarded actions.
const guarded = {
  repo: SHOP, default_branch: "main",
  rules: [
    { id: "migrations", kind: "builtin", label: "Database migrations", detail: "prisma migrate deploy, sqlx migrate run, alembic upgrade, rails db:migrate, …", on: true },
    { id: "deploy", kind: "builtin", label: "Deploy scripts", detail: "scripts/deploy*, kubectl apply, terraform apply, fly deploy, npm publish, …", on: true },
    { id: "push-default", kind: "builtin", label: "git push to the default branch", detail: "a push to main (this repo's default), or a force push", on: true },
    { id: "rm-outside", kind: "builtin", label: "rm -rf outside the worktree", detail: "a recursive rm of anything not under the agent's own worktree (temporary folders aside)", on: true },
    { id: "p1", kind: "prefix", label: "stripe listen --forward-to", detail: "commands that start with these words", on: true },
    { id: "p2", kind: "prefix", label: "npm run seed:prod", detail: "commands that start with these words", on: true },
  ],
};
const diffMigration = [{
  path: "prisma/migrations/0042_saved_carts/migration.sql", status: "added", added: 14, removed: 0,
  lines: [
    "+-- Saved carts survive logout: when a cart was saved, and the user's latest first.",
    "+ALTER TABLE \"carts\" ADD COLUMN \"saved_at\" TIMESTAMP(3);",
    "+",
    "+CREATE INDEX \"carts_user_id_saved_at_idx\" ON \"carts\" (\"user_id\", \"saved_at\" DESC);",
    "+",
    "+-- Carts saved before this migration count as saved when they were last updated.",
    "+UPDATE \"carts\" SET \"saved_at\" = \"updated_at\" WHERE \"items\" <> '[]';",
  ],
}, { path: "prisma/schema.prisma", status: "modified", added: 3, removed: 0, lines: ["   updatedAt DateTime @updatedAt @map(\"updated_at\")", "+  savedAt   DateTime? @map(\"saved_at\")", "+", "+  @@index([userId, savedAt(sort: Desc)])"] }];
const diffPush = [
  { path: "public/.well-known/apple-developer-merchantid-domain-association", status: "added", added: 1, removed: 0, lines: ["+7B227073704964223A2242323138...(Stripe's domain file, 1 line)"] },
  { path: "src/checkout/PaymentRequest.tsx", status: "modified", added: 52, removed: 8, lines: ["@@ -12,6 +12,14 @@ export function PaymentRequestButton({ total }: Props) {", "-  const pr = null;", "+  const pr = useMemo(() => stripe?.paymentRequest({", "+    country: \"US\", currency: \"usd\",", "+    total: { label: \"Shop\", amount: total },", "+    requestPayerEmail: true,", "+  }), [stripe, total]);"] },
];
const cr = (o) => ({
  repo: SHOP, decided: null, decided_ms: null, decided_by: null, reason: null, output: null, exit_code: null, ms: null, told: null, precedent: null, ...o,
  agent_name: o.agent_name, created: iso(o.created_ms),
});
const changes = [
  cr({ id: "c5c0a0001", agent: byName.migrate.id, agent_name: "migrate", cwd: byName.migrate.cwd, branch: "wb/migrate", command: "npx prisma migrate deploy",
    why: "Applies 0042_saved_carts to staging: adds carts.saved_at and an index on (user_id, saved_at), so saved carts survive logout. Additive: nothing is dropped.",
    rule: "migrations", rule_label: "Database migrations", risk: "medium", risk_why: ["changes the staging database's schema", "additive: no DROP or TRUNCATE", "backfills saved_at with one UPDATE"],
    diff: { files: 2, added: 17, removed: 0, list: diffMigration }, state: "open", created_ms: NOW - 4 * 60000,
    precedent: { text: "Migrations on staging were approved twice this week; production runs only from main's CI.", ids: ["c4ec0001:22"] } }),
  cr({ id: "c5c0a0002", agent: byName.payments.id, agent_name: "payments", cwd: byName.payments.cwd, branch: "wb/payments", command: "git push origin wb/payments:main",
    why: "Apple verifies the merchant domain file at /.well-known on the live site, which deploys from main; it must be there before the Apple Pay button can be tested.",
    rule: "push-default", rule_label: "git push to the default branch", risk: "high", risk_why: ["pushes 3 unreviewed commits to main", "main deploys to production on push"],
    diff: { files: 2, added: 53, removed: 8, list: diffPush }, state: "open", created_ms: NOW - 60000,
    precedent: { text: "You decided on 9 Nov: nothing reaches main without a pull request.", ids: ["c4ec0001:30"] } }),
  cr({ id: "c5c0a0003", agent: byName.lead.id, agent_name: "lead", cwd: byName.lead.cwd, branch: "wb/lead", command: "./scripts/deploy.sh staging",
    why: "Deploys the branch to staging so payments can test Apple Pay against a real domain.", rule: "deploy", rule_label: "Deploy scripts", risk: "medium", risk_why: ["deploys to staging"],
    diff: { files: 0, added: 0, removed: 0, list: [] }, state: "done", created_ms: NOW - 52 * 60000, decided_ms: NOW - 51 * 60000, decided: iso(NOW - 51 * 60000), decided_by: "you", exit_code: 0, ms: 48200,
    output: "→ building shop@4e1d0c9 (staging)\n✓ built in 31.2 s\n→ uploading 214 files to staging-shop\n✓ released v2026.11.14-0652 to https://staging.shop.example\n", told: "typed" }),
  cr({ id: "c5c0a0004", agent: byName.cart.id, agent_name: "cart", cwd: byName.cart.cwd, branch: "wb/cart", command: "rm -rf ~/.cache/prisma ../shop/node_modules",
    why: "Prisma's engine cache looks stale after the upgrade; clearing it and the main checkout's modules should fix the client mismatch.", rule: "rm-outside", rule_label: "rm -rf outside the worktree",
    risk: "high", risk_why: ["deletes ../shop/node_modules: the main checkout, not this worktree", "deletes ~/.cache/prisma for every project"],
    diff: { files: 0, added: 0, removed: 0, list: [] }, state: "declined", created_ms: NOW - 23 * 60000, decided_ms: NOW - 22 * 60000, decided: iso(NOW - 22 * 60000), decided_by: "you",
    reason: "Not the main checkout: clear your own node_modules and run npx prisma generate instead.", told: "queued" }),
];

// Model routing: what similar past tasks of this repo did on each model.
function route(task) {
  const t = String(task || "").toLowerCase();
  if (t.trim().split(/\s+/).filter(Boolean).length < 3) return { available: true, similar: 0, apply: false, suggestion: null, alternatives: [], examples: [], why: "Say a few words of the task to see what similar tasks did.", missing: [] };
  const big = /migrat|schema|refactor|architect|payment|auth|design/.test(t);
  const alts = big
    ? [{ model: "opus", n: 6, done: 5 }, { model: "sonnet", n: 8, done: 5 }, { model: "haiku", n: 2, done: 0 }]
    : [{ model: "haiku", n: 11, done: 9 }, { model: "sonnet", n: 7, done: 6 }, { model: "opus", n: 3, done: 3 }];
  const cost = { haiku: 0.4, sonnet: 1.85, opus: 4.9 }, mins = { haiku: 6, sonnet: 14, opus: 22 };
  const alternatives = alts.map((a) => ({ ...a, rate: a.done / a.n, avg_cost_usd: big ? cost[a.model] * 2.2 : cost[a.model], avg_minutes: big ? mins[a.model] * 2 : mins[a.model], avg_tokens: Math.round((big ? 2.2 : 1) * { haiku: 820000, sonnet: 610000, opus: 540000 }[a.model]) }));
  const s = alternatives[0];
  const n = alts.reduce((x, a) => x + a.n, 0);
  return {
    available: true, similar: n, apply: n >= 5, suggestion: { ...s },
    why: big ? `Opus finished ${s.done} of ${s.n} tasks like this; Sonnet ${alts[1].done} of ${alts[1].n}, and its redos cost more than Opus did.` : `Haiku finished ${s.done} of ${s.n} tasks like this, as often as Sonnet, at a fifth of the cost.`,
    alternatives,
    examples: [
      { agent: "a7e01c11", name: "rename-buy-again", task: "Rename the Add to cart button to Buy now on product pages and fix the snapshots", model: "haiku", done: true, cost_usd: 0.31, minutes: 5, ended: iso(NOW - 3 * 864e5), score: 7.4 },
      { agent: "a7e01c12", name: "order-copy", task: "Change the order confirmation copy and update its tests", model: "haiku", done: true, cost_usd: 0.42, minutes: 7, ended: iso(NOW - 5 * 864e5), score: 6.1 },
      { agent: "a7e01c13", name: "label-tests", task: "Rename the Checkout button label and update the e2e tests", model: "sonnet", done: true, cost_usd: 1.62, minutes: 12, ended: iso(NOW - 9 * 864e5), score: 5.8 },
    ],
    default: "Claude Code's default",
    missing: [],
  };
}

const err = (status, code, error) => [status, { error, error_code: code }];
export const routes = [
  ["GET", /^\/api\/time$/, (_, url) => {
    if (url.searchParams.get("repo") && url.searchParams.get("repo") !== SHOP) return { available: false, why: "No snapshot of this repository's work yet: one is kept every 10 minutes while agents work, and at each goal change." };
    const at = url.searchParams.get("at");
    if (at) return asOf(Number(at));
    return { available: true, repo: SHOP, now_ms: Date.now(), from_ms: snapshots[0].ts_ms, keep_ms: 864e5, every_ms: 600000, snapshots, events };
  }],
  ["POST", /^\/api\/time\/snapshot$/, () => { const s = { id: `t${Math.round(Date.now() / 1000)}`, ts_ms: Date.now(), reason: "manual", detail: null }; snapshots.push(s); return [201, { snapshot: s }]; }],
  ["GET", /^\/api\/guarded$/, () => guarded],
  ["PUT", /^\/api\/guarded$/, (_, __, b) => { if (!b || !Array.isArray(b.rules)) return err(400, "bad_request", "rules is required"); guarded.rules = b.rules.map((r) => ({ ...r, id: r.id || "p" + hex8() })); return guarded; }],
  ["GET", /^\/api\/changes$/, (_, url) => {
    const st = url.searchParams.get("state");
    return { requests: changes.filter((c) => !st || c.state === st).sort((a, b) => b.created_ms - a.created_ms) };
  }],
  ["POST", /^\/api\/changes\/([^/]+)\/approve$/, (m) => {
    const c = changes.find((x) => x.id === m[1]);
    if (!c) return err(404, "not_found", `no change request ${m[1]}`);
    if (c.state !== "open") return err(409, "conflict", `${c.id} is ${c.state}`);
    Object.assign(c, { state: "done", decided_by: "you", decided_ms: Date.now(), decided: iso(Date.now()), exit_code: 0, ms: 2400, told: "queued",
      output: c.command.includes("prisma") ? "Environment variables loaded from .env\nPrisma schema loaded from prisma/schema.prisma\nDatasource \"db\": PostgreSQL database \"shop\" at \"staging-db:5432\"\n\n1 migration found in prisma/migrations\n\nApplying migration `0042_saved_carts`\n\nThe following migration(s) have been applied:\n\nmigrations/\n  └─ 0042_saved_carts/\n    └─ migration.sql\n\nAll migrations have been successfully applied.\n" : "Everything up-to-date\n" });
    return { request: c };
  }],
  ["POST", /^\/api\/changes\/([^/]+)\/decline$/, (m, _, b) => {
    const c = changes.find((x) => x.id === m[1]);
    if (!c) return err(404, "not_found", `no change request ${m[1]}`);
    if (c.state !== "open") return err(409, "conflict", `${c.id} is ${c.state}`);
    Object.assign(c, { state: "declined", decided_by: "you", decided_ms: Date.now(), decided: iso(Date.now()), reason: (b && b.reason) || null, told: "queued" });
    return { request: c };
  }],
  ["GET", /^\/api\/route$/, (_, url) => route(url.searchParams.get("task"))],
];

// The shell (app.js) opens a stream; this one sends the shop's fleet once.
export const raw = [
  ["GET", /^\/api\/stream$/, (_, url, req, res) => {
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store" });
    res.write(`event: snapshot\ndata: ${JSON.stringify({ agents: url.searchParams.get("shop") === "0" ? [] : now })}\n\n`);
    const t = setInterval(() => res.write(": keep-alive\n\n"), 15000);
    req.on("close", () => clearInterval(t));
  }],
];
