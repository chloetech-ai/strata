// Preview lab (SPEC.md, "Fourth build", use cases 2 and 3) for a demo repo
// "shop": checkout runs the storefront's dev server (vite on :5173, which
// passes /api to the server the api agent runs on :8080). A persona test is
// playing on it, and four runtime errors came in: one thrown in the page,
// one console warning, one 500 through the preview proxy and the same
// failure as an OTLP log from the API server, each joined to the edit that
// probably caused it.
//
// The shop story is served when the page asks with ?story=shop (the
// harness's address, seen as the Referer); everything else falls through
// to the harbor fixtures, so the other screens are unchanged:
//   /dev/levels.html?screen=preview&agent=b5c0a002&story=shop
import { T, ms, routes as baseRoutes } from "./base.mjs";
import { routes as pvRoutes, raw as pvRaw } from "./preview.mjs";

export const SHOP = "/home/user/src/shop";
const WT = "/home/user/.local/share/chloedb-workbench/worktrees/shop-7e21c9a0";

function agent(id, name, o) {
  return {
    id, name, repo: SHOP, cwd: `${WT}/${name}`, branch: `wb/${name}`, parent: null, depth: 0,
    model: "sonnet", pid: 5100, exit_code: null, ended: null, running: true, children: [],
    session_id: `7a11${id.slice(1)}-0000-4000-8000-000000000000`, base: "c41e9d07b2aa", goal: null, outcome: null,
    preview_url: null, waiting_for: null, ...o,
  };
}
export const shopAgents = [
  agent("b5c0a002", "checkout", { task: "One-page checkout: promo codes, saved addresses and cards", state: "working", state_since: T(1), created: T(52), started_ms: ms(52), narration: "Wiring the saved-card picker into the payment step.", narration_ts: T(0.4), last_event: "Edit src/checkout/PaymentStep.tsx", preview_url: "http://localhost:5173" }),
  agent("b5c0a003", "pricing", { task: "Cart totals in integer cents, with tax and shipping", state: "working", state_since: T(2), created: T(50), started_ms: ms(50), narration: "Running the cart tests: 31 of 32 pass.", narration_ts: T(0.6), last_event: "Bash npm test -- cart" }),
  agent("b5c0a004", "api", { task: "POST /api/orders: validate, reserve stock, charge the card", state: "idle", state_since: T(3), created: T(48), started_ms: ms(48), narration: "Added shipping_cents to orders; the migration is next.", narration_ts: T(3), last_event: "Stop", preview_url: "http://127.0.0.1:8080" }),
  agent("b5c0a005", "search", { task: "Search and filters on the product list", state: "working", state_since: T(1), created: T(44), started_ms: ms(44), narration: "Reading how the product query pages its results.", narration_ts: T(0.2), last_event: "Read src/catalog/query.ts" }),
];
const isShop = (id) => shopAgents.some((a) => a.id === id);
const byId = (id) => shopAgents.find((a) => a.id === id);
const [checkout, pricing, api, search] = shopAgents;
const fromShop = (req) => /[?&]story=shop\b/.test(String(req?.headers?.referer || ""));

// -------------------------------------------------------------- diffs and edits
const hunk = (new_start, src) => {
  let n = new_start;
  return { old_start: new_start, new_start, lines: src.trim().split("\n").map((l) => {
    const kind = l[0] === "+" || l[0] === "-" ? l[0] : " ";
    return { kind, text: l.slice(2), new_line: kind === "-" ? null : n++ };
  }) };
};
const DIFFS = {
  [checkout.id]: [
    { path: "src/lib/money.ts", status: "modified", hunks: [hunk(6, `
  // Prices are integer cents everywhere (decided 12 Nov).
- export const formatMoney = (dollars: number) => "$" + dollars.toFixed(2);
+ export function formatMoney(p: { cents: number }) {
+   return "$" + (p.cents / 100).toFixed(2);
+ }`)] },
    { path: "src/checkout/PromoCode.tsx", status: "added", hunks: [hunk(1, `
+ export function PromoCode({ onApply }: Props) {
+   const [code, setCode] = useState("");
+   return (
+     <div className="promo">
+       <input className="promo-input" placeholder="Promo code" value={code} onChange={(e) => setCode(e.target.value)} />
+       <button className="promo-apply" onClick={() => onApply(code)}>Apply</button>
+     </div>
+   );
+ }`)] },
    { path: "src/checkout/AddressList.tsx", status: "modified", hunks: [hunk(12, `
-   return <ul>{addresses.map((a) => <Address a={a} key={a.id} />)}</ul>;
+   return <ul className="addresses">{addresses.map((a) => <Address a={a} />)}</ul>;`)] },
    { path: "src/checkout/PaymentStep.tsx", status: "modified", hunks: [hunk(30, `
+       <button className="place-order" disabled={placing}>{placing ? "Placing…" : "Place order"}</button>`)] },
  ],
  [api.id]: [
    { path: "src/server/orders.ts", status: "modified", hunks: [hunk(84, `
    const order = await db.orders.insert({
      customer_id: req.user.id,
      total_cents: totals.total_cents,
+     shipping_cents: totals.shipping_cents,
    });`)] },
  ],
  [pricing.id]: [
    { path: "src/cart/totals.ts", status: "modified", hunks: [hunk(10, `
- export const total = (items) => items.reduce((s, i) => s + i.price * i.qty, 0);
+ export const totalCents = (items: Item[]) => items.reduce((s, i) => s + i.price_cents * i.qty, 0);`)] },
  ],
  [search.id]: [],
};
const EDITS = [
  [checkout, "src/checkout/AddressList.tsx", 12], [pricing, "src/cart/totals.ts", 8], [checkout, "src/lib/money.ts", 4.2],
  [checkout, "src/checkout/PromoCode.tsx", 6], [api, "src/server/orders.ts", 3.4], [checkout, "src/checkout/PaymentStep.tsx", 0.5],
];
let seq = 9000;
const EVENTS = [];
for (const [a, file, ago] of EDITS) {
  for (const [kind, dt] of [["PreToolUse", 0.05], ["PostToolUse", 0]]) {
    seq++;
    EVENTS.push({ id: `${a.id}:${String(seq).padStart(8, "0")}`, seq, ts: T(ago + dt), agent: a.id, kind, tool: "Edit", summary: `Edit ${file}`,
      data: JSON.stringify({ hook_event_name: kind, tool_name: "Edit", tool_input: { file_path: `${a.cwd}/${file}` } }) });
  }
}
EVENTS.sort((x, y) => Date.parse(x.ts) - Date.parse(y.ts));

// ------------------------------------------------------------ runtime errors
const frame = (fn, file, line, col, in_repo = true, raw) => ({ fn, file, line, col, in_repo, raw: raw || `at ${fn} (${file}:${line}:${col})` });
const culprit = (a, file, line, editAgo, firstAgo, how, edit_line) => ({
  agent: a, file, line, edit_ms: ms(editAgo), edit_line: edit_line ?? line, before_ms: ms(firstAgo) - ms(editAgo), how, summary: `Edit ${file}`,
  event: `${a.id}:00009${Math.round(editAgo * 10)}`,
});
export const ERRORS = [
  { id: "e3f9a0c11", agent: checkout.id, agent_name: "checkout", repo: SHOP, cwd: checkout.cwd, source: "overlay", level: "error",
    message: "TypeError: Cannot read properties of undefined (reading 'toFixed')",
    stack: "TypeError: Cannot read properties of undefined (reading 'toFixed')\n    at formatMoney (http://localhost:5173/src/lib/money.ts?t=1731568320:8:31)\n    at CartTotal (http://localhost:5173/src/cart/CartTotal.tsx:21:15)\n    at renderWithHooks (http://localhost:5173/node_modules/.vite/deps/react-dom_client.js?v=4b1c:11548:26)",
    frames: [frame("formatMoney", "src/lib/money.ts", 8, 31), frame("CartTotal", "src/cart/CartTotal.tsx", 21, 15), frame("renderWithHooks", "node_modules/.vite/deps/react-dom_client.js", 11548, 26, false)],
    file: "src/lib/money.ts", line: 8, url: "/cart", status: null, count: 14, first_ms: ms(3.1), last_ms: ms(0.3), state: "open", service: null,
    culprit: culprit(checkout, "src/lib/money.ts", 8, 4.2, 3.1, "stack", 7) },
  { id: "e7b20d4e8", agent: checkout.id, agent_name: "checkout", repo: SHOP, cwd: api.cwd, source: "proxy", level: "error",
    message: "500 POST /api/orders: column \"shipping_cents\" of relation \"orders\" does not exist",
    stack: `error: column "shipping_cents" of relation "orders" does not exist\n    at createOrder (${api.cwd}/src/server/orders.ts:88:11)\n    at async handle (${api.cwd}/src/server/router.ts:41:5)`,
    frames: [frame("createOrder", "src/server/orders.ts", 88, 11), frame("handle", "src/server/router.ts", 41, 5)],
    file: "src/server/orders.ts", line: 88, url: "/api/orders", status: 500, count: 3, first_ms: ms(2.2), last_ms: ms(0.4), state: "open", service: null,
    culprit: culprit(api, "src/server/orders.ts", 88, 3.4, 2.2, "stack", 87) },
  { id: "e1c6d8a90", agent: api.id, agent_name: "api", repo: SHOP, cwd: api.cwd, source: "otlp", level: "error",
    message: "insert into orders failed: column \"shipping_cents\" does not exist",
    stack: `PostgresError: column "shipping_cents" does not exist\n    at createOrder (${api.cwd}/src/server/orders.ts:88:11)`,
    frames: [frame("createOrder", "src/server/orders.ts", 88, 11)],
    file: "src/server/orders.ts", line: 88, url: null, status: null, count: 3, first_ms: ms(2.2), last_ms: ms(0.4), state: "open", service: "shop-api",
    culprit: culprit(api, "src/server/orders.ts", 88, 3.4, 2.2, "stack", 87) },
  { id: "e05a7f3b2", agent: checkout.id, agent_name: "checkout", repo: SHOP, cwd: checkout.cwd, source: "console", level: "warn",
    message: "Warning: Each child in a list should have a unique \"key\" prop. Check the render method of `AddressList`.",
    stack: "    at Address (http://localhost:5173/src/checkout/Address.tsx:5:3)\n    at AddressList (http://localhost:5173/src/checkout/AddressList.tsx:13:12)",
    frames: [frame("Address", "src/checkout/Address.tsx", 5, 3), frame("AddressList", "src/checkout/AddressList.tsx", 13, 12)],
    file: "src/checkout/AddressList.tsx", line: 13, url: "/checkout", status: null, count: 2, first_ms: ms(11.5), last_ms: ms(1.2), state: "open", service: null,
    culprit: culprit(checkout, "src/checkout/AddressList.tsx", 13, 12, 11.5, "stack", 13) },
];

// ------------------------------------------------------------------ personas
export const PERSONAS = [
  { id: "first-timer", name: "Maya Chen", role: "a first-time visitor on her phone", traits: ["Skims; taps the biggest button.", "Gives up after two things go wrong."], device: "phone" },
  { id: "keyboard", name: "Ken Okafor", role: "a power user who uses only the keyboard", traits: ["Tabs through every form.", "Expects Enter to submit."], device: "desktop" },
  { id: "careful", name: "Ruth Alvarez", role: "71, careful, reads every label", traits: ["Reads prices twice.", "Distrusts anything that changes without asking."], device: "desktop" },
  { id: "hurried", name: "Sam Patel", role: "in a hurry between meetings", traits: ["Double-clicks.", "Uses the back button a lot."], device: "desktop" },
  { id: "edge-cases", name: "Ines Duarte", role: "a QA engineer trying edge cases", traits: ["Empty fields, long text, emoji, negative numbers.", "Reloads in the middle of things."], device: "desktop" },
];
const P = Object.fromEntries(PERSONAS.map((p) => [p.id, p]));

const step = (n, action, input, thought, url, agoMin, extra = {}) => ({ step: n, action, input, thought, ok: true, error: null, url, events: [], ms: 900 + n * 37, ts_ms: ms(agoMin), ...extra });
const RUN_STEPS = [
  step(1, "look", {}, "Seeing what's on the front page", "/", 2.9),
  step(2, "click", { ref: "e14", name: "Add to cart: Linen tote" }, "Adding the tote, that's what I came for", "/", 2.7),
  step(3, "click", { ref: "e31", name: "Increase quantity" }, "I want two of them", "/cart", 2.5),
  step(4, "type", { ref: "e40", name: "Promo code", text: "SAVE10" }, "Putting in my code from the email", "/cart", 2.3),
  step(5, "click", { ref: "e41", name: "Apply" }, "Applying the code", "/cart", 2.1, { events: [{ kind: "exception", text: "TypeError: Cannot read properties of undefined (reading 'toFixed')" }] }),
  step(6, "look", {}, "The total just says a dash now, that's odd", "/cart", 1.9),
  step(7, "scroll", { direction: "down" }, "Looking for the total somewhere else", "/cart", 1.6),
  step(8, "click", { ref: "e52", name: "Checkout" }, "Going on anyway, maybe it shows at the end", "/checkout", 1.3),
  step(9, "click", { ref: "e60", name: "Use saved card •••• 4242" }, "Using my saved card", "/checkout", 1.0),
  step(10, "click", { ref: "e66", name: "Place order" }, "Placing the order", "/checkout", 0.6, { events: [{ kind: "http", text: "500 POST /api/orders" }] }),
  step(11, "wait", { seconds: 3 }, "It says Placing… and nothing happens", "/checkout", 0.2),
];
export const RUNS = [
  { id: "p4e1a7c20", agent: checkout.id, agent_name: "checkout", repo: SHOP, persona: P["first-timer"], task: "Buy two linen totes with the code SAVE10 and pay with the saved card", model: "haiku", adapter: "claude -p", by: "you",
    state: "running", outcome: null, answer: null, step_count: 11, max_steps: 25, findings_count: 2, error: null, cost_usd: null, created: T(3), created_ms: ms(3), ended: null, ended_ms: null, steps: RUN_STEPS },
  { id: "p2b9d0e14", agent: checkout.id, agent_name: "checkout", repo: SHOP, persona: P.keyboard, task: "Check out using only the keyboard", model: "haiku", adapter: "claude -p", by: "you",
    state: "done", outcome: "gave_up", answer: "Couldn't reach the promo code field with Tab, so I stopped.", step_count: 14, max_steps: 25, findings_count: 2, error: null, cost_usd: null, created: T(26), created_ms: ms(26), ended: T(23), ended_ms: ms(23), steps: [] },
  { id: "p0c4f8a33", agent: checkout.id, agent_name: "checkout", repo: SHOP, persona: P.careful, task: "Change the delivery address to a new one and check the shipping price", model: "sonnet", adapter: "claude -p", by: search.id,
    state: "done", outcome: "done", answer: "Added 14 Elm St; shipping stayed $5.00, which matches the banner.", step_count: 9, max_steps: 25, findings_count: 0, error: null, cost_usd: null, created: T(41), created_ms: ms(41), ended: T(39), ended_ms: ms(39), steps: [] },
];
export const FINDINGS = [
  { id: "f8d21c0a4", run: "p4e1a7c20", agent: checkout.id, repo: SHOP, kind: "bug", severity: 3, title: "The total turns into “—” after applying a promo code",
    detail: "After Apply with SAVE10 on /cart the total shows “—” and the page threw TypeError: Cannot read properties of undefined (reading 'toFixed'). Maya went on without knowing what she would pay.",
    step: 5, url: "/cart", state: "sent", question: null, steer: "s4c1e0b77", created_ms: ms(2.0), updated_ms: ms(1.1) },
  { id: "f3a90e6b1", run: "p4e1a7c20", agent: checkout.id, repo: SHOP, kind: "bug", severity: 3, title: "Placing an order fails silently",
    detail: "POST /api/orders answered 500; the page shows no message and the button stays on “Placing…”. Maya can't tell whether she was charged.",
    step: 10, url: "/checkout", state: "open", question: null, steer: null, created_ms: ms(0.5), updated_ms: ms(0.5) },
  { id: "f6b0c2d77", run: "p2b9d0e14", agent: checkout.id, repo: SHOP, kind: "confusing_ui", severity: 2, title: "The promo code field can't be reached with Tab",
    detail: "Tab goes from the quantity buttons straight to Checkout; the promo input has tabindex=-1.", step: 6, url: "/cart", state: "asked", question: "q9e0a1b2c", steer: null, created_ms: ms(24), updated_ms: ms(22) },
  { id: "f1e7a5c09", run: "p2b9d0e14", agent: checkout.id, repo: SHOP, kind: "confusing_ui", severity: 1, title: "Focus jumps to the top after Apply",
    detail: "After Enter in the promo field the focus lands on the logo, and the screen reader reads the header again.", step: 9, url: "/cart", state: "open", question: null, steer: null, created_ms: ms(23.5), updated_ms: ms(23.5) },
];

// ------------------------------------------------------------------ the app
const PAGE = `<!doctype html><html><head><meta charset="utf-8"><title>Shop · Cart</title>
<style>
*{box-sizing:border-box}body{margin:0;background:#fbfaf7;color:#1d1b18;font:14px/1.5 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
header{display:flex;align-items:center;gap:22px;padding:16px 34px;border-bottom:1px solid #ece8df;background:#fff}
.logo{font-weight:800;letter-spacing:-.02em;font-size:18px}.logo i{color:#d4572a;font-style:normal}
nav{display:flex;gap:18px;color:#6d675c}.cart-link{margin-left:auto;font-weight:650}
.wrap{display:grid;grid-template-columns:1fr 320px;gap:28px;padding:26px 34px;max-width:1080px}
h1{font-size:22px;margin:0 0 14px;letter-spacing:-.01em}
.item{display:grid;grid-template-columns:64px 1fr auto auto;gap:14px;align-items:center;padding:14px 0;border-bottom:1px solid #ece8df}
.thumb{width:64px;height:64px;border-radius:10px;background:linear-gradient(135deg,#e9dfcc,#d8c7a6)}.thumb.mug{background:linear-gradient(135deg,#cfd8dc,#9fb3bd)}
.name{font-weight:650}.sub{color:#8a8376;font-size:12.5px}
.qty{display:flex;align-items:center;border:1px solid #ddd6c8;border-radius:8px}.qty button{border:0;background:none;width:28px;height:28px;font-size:15px}.qty span{width:22px;text-align:center}
.price{font-weight:650;width:76px;text-align:right}
.summary{background:#fff;border:1px solid #ece8df;border-radius:12px;padding:18px}
.summary h2{font-size:13px;text-transform:uppercase;letter-spacing:.06em;color:#8a8376;margin:0 0 12px}
.row{display:flex;justify-content:space-between;padding:5px 0}.row.disc{color:#2c7a4b}.row.total{border-top:1px solid #ece8df;margin-top:8px;padding-top:12px;font-weight:750;font-size:17px}
.promo{display:flex;gap:8px;margin:14px 0}.promo-input{flex:1;border:1px solid #ddd6c8;border-radius:8px;padding:8px 10px;font:inherit}
.promo-apply{border:1px solid #1d1b18;background:#fff;border-radius:8px;padding:8px 12px;font-weight:650}
.checkout-btn{width:100%;border:0;border-radius:10px;padding:12px;background:#d4572a;color:#fff;font:inherit;font-weight:700;font-size:15px;margin-top:14px}
.note{color:#8a8376;font-size:12px;margin-top:10px}
</style></head><body>
<header><span class="logo">shop<i>.</i></span><nav><span>New in</span><span>Kitchen</span><span>Bags</span><span>Gifts</span></nav><span class="cart-link">Cart (3)</span></header>
<div class="wrap"><div>
<h1>Your cart</h1>
<div class="item"><div class="thumb"></div><div><div class="name">Linen tote</div><div class="sub">Natural · 38 × 42 cm</div></div><div class="qty"><button>−</button><span>2</span><button>+</button></div><div class="price">$68.00</div></div>
<div class="item"><div class="thumb mug"></div><div><div class="name">Stoneware mug</div><div class="sub">Slate · 350 ml</div></div><div class="qty"><button>−</button><span>1</span><button>+</button></div><div class="price">$18.00</div></div>
</div>
<aside class="summary"><h2>Summary</h2>
<div class="row"><span>Subtotal</span><span>$86.00</span></div>
<div class="row disc"><span>SAVE10</span><span>−$8.60</span></div>
<div class="row"><span>Shipping</span><span>$5.00</span></div>
<div class="row total cart-total"><span>Total</span><span>—</span></div>
<div class="promo"><input class="promo-input" value="SAVE10" aria-label="Promo code"><button class="promo-apply">Apply</button></div>
<button class="checkout-btn">Checkout</button>
<div class="note">Free returns within 30 days.</div>
</aside></div>
<script>history.replaceState(null, "", location.pathname.replace(/\\/$/, "") + "/cart");</script>
<script src="/preview-overlay.js"></script>
</body></html>`;

// ---------------------------------------------------- the routes, and the fall-through
const json = (res, status, v) => { res.writeHead(status, { "content-type": "application/json" }); res.end(JSON.stringify(v)); };
const findRoute = (list, method, src) => list.find(([m, re]) => m === method && re.source === src);
const baseList = findRoute(baseRoutes, "GET", "^\\/api\\/agents$");
const baseOne = findRoute(baseRoutes, "GET", "^\\/api\\/agents\\/([^/]+)$");
// The shop's agents through the harbor fixtures' own routes (the levels
// mock reads each route's function when a request comes, so these wrap).
if (baseOne) { const f = baseOne[2]; baseOne[2] = (m, url, body) => (isShop(m[1]) ? { agent: byId(m[1]) } : f(m, url, body)); }
for (const [src, fn] of [
  ["^\\/api\\/agents\\/([^/]+)\\/diff$", (id) => ({ base: "c41e9d07b2aa", files: DIFFS[id] || [] })],
  ["^\\/api\\/agents\\/([^/]+)\\/events$", (id, url) => {
    const withData = url.searchParams.get("data") === "1";
    return { events: EVENTS.filter((e) => e.agent === id).map(({ data, ...e }) => (withData ? { ...e, data } : e)) };
  }],
]) {
  const r = findRoute(pvRoutes, "GET", src);
  if (r) { const f = r[2]; r[2] = (m, url, body) => (isShop(m[1]) ? fn(m[1], url) : f(m, url, body)); }
}
const pvPage = pvRaw.find(([m, re]) => m === "GET" && re.source.startsWith("^\\/preview\\/"));
if (pvPage) {
  const f = pvPage[2];
  pvPage[2] = (m, url, req, res) => {
    if (!isShop(m[1])) return f(m, url, req, res);
    res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    res.end(PAGE);
  };
}

const runOf = (id) => RUNS.find((r) => r.id === id);
const lite = ({ steps, ...r }) => r;
export const routes = [
  ["GET", /^\/api\/runtime\/errors$/, (_, url) => {
    const agentQ = url.searchParams.get("agent"), repo = url.searchParams.get("repo");
    const list = ERRORS.filter((e) => (!agentQ || e.agent === agentQ) && (!repo || e.repo === repo) && e.state === "open");
    return { errors: list, total: list.length, missing: [], otlp: { endpoint: "http://127.0.0.1:7740/v1/logs", env: "OTEL_EXPORTER_OTLP_ENDPOINT=$CHLOEDB_WORKBENCH_URL" } };
  }],
  ["POST", /^\/api\/runtime\/errors\/([^/]+)\/resolve$/, (m) => { const e = ERRORS.find((x) => x.id === m[1]); if (!e) return [404, { error: "no such error", error_code: "not_found" }]; e.state = "resolved"; return { error: e }; }],
  ["GET", /^\/api\/swarm\/personas$/, () => ({ personas: PERSONAS, available: true, why: null, adapter: "claude -p", models: ["haiku", "sonnet", "opus"], default_model: "haiku" })],
  ["GET", /^\/api\/swarm\/runs$/, (_, url) => {
    const a = url.searchParams.get("agent");
    return { runs: RUNS.filter((r) => !a || r.agent === a).map(lite), available: true, why: null };
  }],
  ["GET", /^\/api\/swarm\/runs\/([^/]+)$/, (m) => {
    const r = runOf(m[1]);
    return r ? { run: r, findings: FINDINGS.filter((f) => f.run === r.id) } : [404, { error: "no such run", error_code: "not_found" }];
  }],
  ["POST", /^\/api\/swarm\/runs$/, (_, __, body) => {
    const a = byId(body?.agent);
    if (!a) return [404, { error: "no such agent", error_code: "not_found" }];
    const persona = P[body?.persona] || { id: "custom", name: "Someone", role: String(body?.persona_text || "a visitor"), traits: [], device: "desktop" };
    const run = { ...RUNS[0], id: `p${Math.random().toString(16).slice(2, 10)}`, persona, task: body?.task || "", state: "starting", step_count: 0, findings_count: 0, steps: [], created: new Date().toISOString(), created_ms: Date.now() };
    RUNS.unshift(run);
    return [201, { run, available: true }];
  }],
  ["POST", /^\/api\/swarm\/runs\/([^/]+)\/stop$/, (m) => { const r = runOf(m[1]); if (!r) return [404, { error: "no such run" }]; r.state = "stopped"; r.outcome = "stopped"; return { run: r }; }],
  ["POST", /^\/api\/swarm\/findings\/([^/]+)\/(send|ask|dismiss)$/, (m) => {
    const f = FINDINGS.find((x) => x.id === m[1]);
    if (!f) return [404, { error: "no such finding", error_code: "not_found" }];
    f.state = { send: "sent", ask: "asked", dismiss: "dismissed" }[m[2]];
    f.updated_ms = Date.now();
    return { finding: f, delivered: m[2] === "send" ? "queued" : undefined };
  }],
];

// /api/agents for the shop story (a raw route: it needs the Referer), else
// the harbor fixture's own answer.
export const raw = [
  ["GET", /^\/api\/agents$/, (m, url, req, res) => {
    if (fromShop(req)) return json(res, 200, { agents: shopAgents });
    return json(res, 200, baseList[2](m, url, null));
  }],
];
