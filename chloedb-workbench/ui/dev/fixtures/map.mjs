// GET /api/map for the harbor story (SPEC.md, "Map"), and the steer route the
// Map and Preview screens send through. The tree is what `git ls-files` of
// the main checkout gives with line counts; agents' files are their diffs
// from `base`, `reading` what they read in the last 10 minutes.
import { T, byName, agents } from "./base.mjs";

// [path, lines] of the main checkout: about 60 files.
export const FILES = [
  ["src/ledger/post.rs", 412], ["src/ledger/batch.rs", 356], ["src/ledger/entries.rs", 288], ["src/ledger/lock.rs", 120], ["src/ledger/mod.rs", 64],
  ["src/refunds/service.rs", 340], ["src/refunds/api.rs", 262], ["src/refunds/limits.rs", 150], ["src/refunds/validate.rs", 118], ["src/refunds/status.rs", 90], ["src/refunds/mod.rs", 40],
  ["src/webhooks/events.rs", 260], ["src/webhooks/retry.rs", 220], ["src/webhooks/sign.rs", 180], ["src/webhooks/mod.rs", 30],
  ["src/charges/capture.rs", 380], ["src/charges/batch.rs", 300], ["src/charges/model.rs", 210], ["src/charges/mod.rs", 32],
  ["src/auth/session.rs", 240], ["src/auth/keys.rs", 180], ["src/auth/mod.rs", 22],
  ["src/payouts/schedule.rs", 320], ["src/payouts/transfer.rs", 260], ["src/payouts/mod.rs", 26],
  ["src/db/pool.rs", 120], ["src/db/migrate.rs", 92],
  ["src/main.rs", 140], ["src/error.rs", 132], ["src/config.rs", 110], ["src/lib.rs", 60],
  ["web/src/dashboard/Dashboard.tsx", 220], ["web/src/dashboard/ChargeRow.tsx", 182], ["web/src/dashboard/RefundLines.tsx", 140], ["web/src/dashboard/RefundButton.tsx", 92], ["web/src/dashboard/BalanceRow.tsx", 60],
  ["web/src/settings/Settings.tsx", 240], ["web/src/settings/ApiKeys.tsx", 160], ["web/src/settings/Team.tsx", 140],
  ["web/src/lib/api.ts", 200], ["web/src/lib/http.ts", 96], ["web/src/lib/money.ts", 80], ["web/src/lib/format.ts", 62],
  ["web/src/App.tsx", 120], ["web/src/main.tsx", 30], ["web/package.json", 44],
  ["migrations/0040_charges.sql", 62], ["migrations/0041_payouts.sql", 48], ["migrations/0042_refund_idem.sql", 34],
  ["tests/refunds/idempotency.rs", 210], ["tests/refunds/api.rs", 180], ["tests/refunds/limits.rs", 140],
  ["tests/charges/capture.rs", 220], ["tests/charges/batch.rs", 160], ["tests/common/mod.rs", 90],
  ["docs/api.md", 340], ["docs/CHANGELOG.md", 200], ["docs/refunds.md", 120],
  ["README.md", 140], ["Cargo.toml", 60],
];

function build(files) {
  const root = { name: "harbor", path: "", lines: 0, children: [] };
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
  // Small files folded into "…", as the server does.
  root.children.push({ name: "…", path: "…", lines: 118, folded: 9 });
  const sum = (n) => (n.children ? (n.lines = n.children.reduce((s, c) => s + sum(c), 0)) : n.lines);
  sum(root);
  const sort = (n) => { if (n.children) { n.children.sort((a, b) => b.lines - a.lines); n.children.forEach(sort); } };
  sort(root);
  return root;
}
export const tree = build(FILES);

const id = (name) => byName[name].id;
export const changes = {
  [id("ledger")]: {
    files: [
      { path: "src/ledger/post.rs", added: 38, removed: 12 }, { path: "src/ledger/batch.rs", added: 54, removed: 20 },
      { path: "src/ledger/lock.rs", added: 22, removed: 4 }, { path: "migrations/0042_refund_idem.sql", added: 34, removed: 0 },
      { path: "tests/refunds/idempotency.rs", added: 61, removed: 5 }, { path: "web/src/dashboard/BalanceRow.tsx", added: 9, removed: 3 },
    ],
    reading: ["src/charges/batch.rs", "src/ledger/entries.rs"],
  },
  [id("refunds-api")]: {
    files: [
      { path: "src/refunds/service.rs", added: 72, removed: 18 }, { path: "src/refunds/api.rs", added: 48, removed: 9 },
      { path: "src/refunds/limits.rs", added: 31, removed: 2 }, { path: "src/refunds/validate.rs", added: 40, removed: 0 },
      { path: "src/refunds/status.rs", added: 22, removed: 0 }, { path: "tests/refunds/limits.rs", added: 44, removed: 0 },
      { path: "tests/refunds/api.rs", added: 36, removed: 3 },
    ],
    reading: ["src/charges/model.rs"],
  },
  [id("web")]: {
    files: [
      { path: "web/src/dashboard/RefundLines.tsx", added: 64, removed: 12 }, { path: "web/src/dashboard/RefundButton.tsx", added: 18, removed: 4 },
      { path: "web/src/dashboard/ChargeRow.tsx", added: 12, removed: 6 }, { path: "web/src/lib/money.ts", added: 8, removed: 2 },
    ],
    reading: ["src/refunds/api.rs", "web/src/lib/api.ts"],
  },
  [id("hooks")]: {
    files: [
      { path: "src/webhooks/events.rs", added: 58, removed: 6 }, { path: "src/webhooks/retry.rs", added: 26, removed: 8 },
      { path: "src/webhooks/sign.rs", added: 14, removed: 3 }, { path: "src/ledger/post.rs", added: 6, removed: 1 },
    ],
    reading: ["src/charges/capture.rs", "src/webhooks/mod.rs"],
  },
  [id("docs")]: {
    files: [{ path: "docs/CHANGELOG.md", added: 24, removed: 0 }, { path: "docs/refunds.md", added: 40, removed: 6 }],
    reading: [],
  },
};

export const collisions = [
  { path: "src/ledger/post.rs", agents: [id("ledger"), id("hooks")], kind: "both_changed",
    detail: "ledger is rewriting post_entry() for idempotency; hooks added an event emit inside it." },
  { path: "src/refunds/api.rs", agents: [id("refunds-api"), id("web")], kind: "heading_into",
    detail: "refunds-api changed the refund response; web read it 4 min ago and is coding against the old shape." },
];

const rec = (rid, kind, text, ts) => ({ id: rid, kind, text, ts });
export const pins = [
  { path: "src/ledger/post.rs", line: 118, record: rec("m4411", "decision", "Money is i64 cents, never floats: a float rounding bug double-counted 1¢ per charge in the batch.", T(60 * 24 * 11)), agent: byName.ledger },
  { path: "src/ledger/batch.rs", line: 64, record: rec("m4502", "decision", "Take the charges lock before posting a refund batch, the fix for the double payout of 4 Nov.", T(31)), agent: byName.ledger },
  { path: "src/refunds/limits.rs", line: null, record: rec("m3920", "preference", "Refund limits come from the merchant's plan, never from a constant.", T(60 * 24 * 6)), agent: null },
  { path: "web/src/dashboard/RefundLines.tsx", line: 22, record: rec("m4120", "decision", "Refund lines nest under their charge, as you chose on 10 Nov.", T(60 * 24 * 4)), agent: byName.web },
  { path: "migrations/0042_refund_idem.sql", line: null, record: rec("m4517", "fact", "Idempotency keys are unique per merchant and kept for 24 hours.", T(22)), agent: byName.ledger },
  { path: "src/webhooks/retry.rs", line: 40, record: rec("m3301", "decision", "Webhook retries back off 1, 5 and 25 minutes, 5 attempts, like charge.updated.", T(60 * 24 * 8)), agent: byName.hooks },
];

export const steers = [];

export const routes = [
  ["GET", /^\/api\/map$/, () => ({
    tree,
    agents: agents.filter((a) => changes[a.id]).map((a) => ({ agent: a, ...changes[a.id] })),
    collisions,
    pins,
  })],
  ["POST", /^\/api\/agents\/([^/]+)\/steer$/, (m, _, body) => {
    const a = agents.find((x) => x.id === m[1]);
    if (!a) return [404, { error: "no such agent", error_code: "not_found" }];
    if (!body || !body.text) return [400, { error: "text is required", error_code: "bad_request" }];
    steers.push({ agent: a.id, text: body.text, ts: new Date().toISOString() });
    return { ok: true };
  }],
];
