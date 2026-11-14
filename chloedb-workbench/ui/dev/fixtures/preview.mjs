// Preview for the harbor story (SPEC.md, "Preview"): web runs the dashboard's
// dev server, and /preview/<agent>/ answers a small charge page with the
// overlay script injected, as the real proxy does. Also the agents' diffs,
// their events, PUT /preview and POST /api/preview/route.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { T, agents, byName } from "./base.mjs";
import { changes, pins } from "./map.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
byName.web.preview_url = "http://localhost:5173";

// ---------------------------------------------------------------- diffs
const hunk = (old_start, new_start, src) => {
  let n = new_start;
  const lines = src.trim().split("\n").map((l) => {
    const kind = l[0] === "+" || l[0] === "-" ? l[0] : " ";
    const text = l.slice(2);
    return { kind, text, new_line: kind === "-" ? null : n++ };
  });
  return { old_start, new_start, lines };
};
const DIFFS = {
  [byName.web.id]: [
    { path: "web/src/dashboard/RefundLines.tsx", status: "modified", hunks: [hunk(14, 14, `
  export function RefundLines({ charge }: Props) {
-   return <RefundTable refunds={charge.refunds} />;
+   // Refund lines nest under their charge (decided 10 Nov).
+   return charge.refunds.map((r) => (
+     <tr className="refund-line" key={r.id}>
+       <td className="indent">↳ Partial refund</td>
+       <td>{money(-r.amount_cents)}</td>
+       <td className={\`status status-\${r.status}\`}>{r.status}</td>
+       <td>{r.source}</td>
+       <td>{when(r.created)}</td>
+     </tr>
+   ));
  }`)] },
    { path: "web/src/dashboard/RefundButton.tsx", status: "modified", hunks: [hunk(8, 8, `
  export function RefundButton({ charge, onRefund }: Props) {
-   return <button onClick={() => onRefund(charge.amount)}>Refund</button>;
+   const left = charge.amount_cents - refunded(charge);
+   return (
+     <button className="refund-btn" disabled={left <= 0} onClick={() => onRefund(left)}>
+       Refund
+     </button>
+   );
  }`)] },
    { path: "web/src/dashboard/ChargeRow.tsx", status: "modified", hunks: [hunk(30, 30, `
-     <tr>
+     <tr className="charge-row">
-       <td>Charge</td>
+       <td><b>Charge</b></td>
        <td>{money(charge.amount_cents)}</td>
+       <td className="status status-captured">{charge.status}</td>`)] },
    { path: "web/src/lib/money.ts", status: "modified", hunks: [hunk(1, 1, `
- export const money = (x: number) => "$" + x.toFixed(2);
+ export const money = (cents: number) =>
+   (cents < 0 ? "−$" : "$") + (Math.abs(cents) / 100).toFixed(2);`)] },
  ],
  [byName["refunds-api"].id]: [
    { path: "src/refunds/status.rs", status: "added", hunks: [hunk(0, 1, `
+ #[derive(Clone, Copy, Debug, Serialize)]
+ #[serde(rename_all = "lowercase")]
+ pub enum RefundStatus {
+     /// Accepted, not yet settled by the processor: shown as "pending".
+     Pending,
+     Paid,
+     Failed,
+ }`)] },
    { path: "src/refunds/api.rs", status: "modified", hunks: [hunk(40, 40, `
  pub struct RefundOut {
      pub id: String,
-     pub amount: f64,
+     pub amount_cents: i64,
+     pub status: RefundStatus,
+     pub source: Source,
  }`)] },
  ],
  [byName.ledger.id]: [
    { path: "web/src/dashboard/BalanceRow.tsx", status: "modified", hunks: [hunk(4, 4, `
  export function BalanceRow({ charge }: Props) {
-   const balance = charge.amount - sum(charge.refunds.map((r) => r.amount));
+   // i64 cents from the ledger's totals, never floats.
+   const balance = charge.ledger.balance_cents;
+   return <tr className="balance-row"><td><b>Balance</b></td><td><b>{money(balance)}</b></td></tr>;
  }`)] },
    { path: "src/ledger/post.rs", status: "modified", hunks: [hunk(112, 112, `
  pub fn post_entry(tx: &mut Tx, refund: &Refund) -> Result<EntryId> {
-     let amount = -(refund.amount as f64);
+     let amount_cents: i64 = -(refund.amount_cents);
+     let key = refund.idempotency_key.as_deref().ok_or(Error::NoKey)?;
+     if let Some(id) = entries::by_key(tx, key)? { return Ok(id); }`)] },
  ],
};
function diffOf(id) {
  const own = DIFFS[id] || [];
  const synth = (changes[id]?.files || []).filter((f) => !own.some((o) => o.path === f.path)).map((f) => ({
    path: f.path, status: "modified",
    hunks: [hunk(20, 20, Array.from({ length: Math.min(f.removed, 3) }, (_, i) => `- // old ${i + 1}`)
      .concat(Array.from({ length: Math.min(f.added, 6) }, (_, i) => `+ // change ${i + 1} of ${f.added}`)).join("\n"))],
  }));
  return { base: "9f1c2ab7e0d4", files: [...own, ...synth] };
}

// --------------------------------------------------------------- events
const EDITS = [
  [byName.web, "web/src/dashboard/RefundLines.tsx", 4], [byName.web, "web/src/dashboard/RefundButton.tsx", 12],
  [byName.web, "web/src/dashboard/ChargeRow.tsx", 58],
  [byName["refunds-api"], "src/refunds/status.rs", 9], [byName["refunds-api"], "src/refunds/api.rs", 15],
  [byName.ledger, "web/src/dashboard/BalanceRow.tsx", 26], [byName.ledger, "src/ledger/post.rs", 7],
];
let seq = 1000;
const EVENTS = [];
for (const [a, file, ago] of EDITS) {
  for (const [kind, dt] of [["PreToolUse", 0.05], ["PostToolUse", 0]]) {
    seq++;
    EVENTS.push({ id: `${a.id}:${String(seq).padStart(8, "0")}`, seq, ts: T(ago + dt), agent: a.id, kind, tool: "Edit", summary: `Edit ${file}`,
      data: JSON.stringify({ hook_event_name: kind, tool_name: "Edit", tool_input: { file_path: `${a.cwd}/${file}` } }) });
  }
}
for (const a of agents.filter((x) => x.running)) {
  seq++;
  EVENTS.push({ id: `${a.id}:${String(seq).padStart(8, "0")}`, seq, ts: T(1), agent: a.id, kind: "PostToolUse", tool: "Bash", summary: a.last_event, data: "{}" });
}
EVENTS.sort((x, y) => Date.parse(x.ts) - Date.parse(y.ts));

// ----------------------------------------------------------------- route
const RECORDS = [
  { id: "m4130", kind: "preference", text: "The Refund button refunds what is left of the charge, never more.", ts: T(60 * 24 * 4), path: "web/src/dashboard/RefundButton.tsx" },
  { id: "m4411", kind: "decision", text: "Money is i64 cents, never floats.", ts: T(60 * 24 * 11), path: "web/src/dashboard/BalanceRow.tsx" },
  { id: "m4508", kind: "decision", text: "A refund is \"pending\" until the processor settles it; the dashboard shows it in amber.", ts: T(9), path: "src/refunds/status.rs" },
];
function route(agentId, pick) {
  // As the UI's overlay matching does: a quoted text beats a class name, and
  // a class counts only where a line sets it.
  const html = String(pick?.html || "");
  const classes = new Set([...(html.match(/class="([^"]*)"/)?.[1] || "").split(/\s+/), ...String(pick?.selector || "").split(/[.#\s>]+/).slice(1)].filter((c) => c.length >= 3));
  const text = String(pick?.text || "").trim().replace(/^↳\s*/, "");
  let best = null;
  for (const a of agents) {
    for (const f of diffOf(a.id).files) {
      let score = 0;
      for (const l of f.hunks.flatMap((h) => h.lines.filter((x) => x.kind === "+").map((x) => x.text))) {
        const set = new Set([...l.matchAll(/class(?:Name)?=\{?["'`]([^"'`]+)["'`]/g)].flatMap((m) => m[1].split(/\s+/)));
        for (const c of classes) if (set.has(c)) score = Math.max(score, 2 + Math.min(c.length, 20) / 20);
        if (text.length >= 3 && (l.includes(`>${text}<`) || l.includes(`"${text}"`))) score = Math.max(score, 3.2);
      }
      if (score && (!best || score > best.score)) best = { agent: a, files: [f.path], score };
    }
  }
  const owner = best ? best.agent : agents.find((a) => a.id === agentId);
  const files = best ? best.files : [];
  const related = [
    ...RECORDS.filter((r) => files.includes(r.path)),
    ...pins.filter((p) => files.some((f) => path.dirname(f) === path.dirname(p.path))).map((p) => p.record),
  ].filter((r, i, all) => all.findIndex((x) => x.id === r.id) === i).map(({ path: _p, ...r }) => r);
  return { owner, files, related };
}

// ------------------------------------------------------------ the app page
const PAGE = `<!doctype html><html><head><meta charset="utf-8"><title>Harbor · ch_8812</title>
<style>
*{box-sizing:border-box}body{margin:0;background:#f6f7f9;color:#1b1f24;font:14px/1.45 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
.app{padding:28px 34px;max-width:1100px}
.crumbs{color:#6b7280;font-size:12.5px}.crumbs a{color:#2f5bd3;text-decoration:none}
h1{font-size:22px;margin:6px 0 2px;letter-spacing:-.01em}.meta{color:#6b7280}
.actions{display:flex;gap:10px;margin-top:16px}
button{font:inherit;border-radius:8px;padding:8px 15px;font-weight:650;cursor:pointer}
.refund-btn{background:#2f5bd3;color:#fff;border:1px solid #2f5bd3}.receipt-btn{background:#fff;border:1px solid #d6dae0;color:#1b1f24}
.card{margin-top:26px;background:#fff;border:1px solid #e3e6ea;border-radius:10px;overflow:hidden}
table{width:100%;border-collapse:collapse}th{background:#fafbfc;color:#6b7280;font-size:11px;font-weight:650;text-transform:uppercase;letter-spacing:.04em;text-align:left;padding:11px 16px}
td{padding:12px 16px;border-top:1px solid #eef0f3}.refund-line td{background:#fbfcfd;border-top:1px dashed #eef0f3;color:#4b5563}
.indent{padding-left:40px}.status-captured,.status-paid{color:#12a150}.status-pending{color:#c98a00}
.balance-row td{background:#fafbfc}
.foot{margin-top:16px;color:#6b7280}
.side{display:grid;grid-template-columns:1fr 1fr;gap:16px;margin-top:22px}
.box{background:#fff;border:1px solid #e3e6ea;border-radius:10px;padding:14px 16px}.box h3{margin:0 0 6px;font-size:12px;text-transform:uppercase;letter-spacing:.05em;color:#6b7280}
.kv{display:grid;grid-template-columns:120px 1fr;gap:4px 10px;font-size:13px}.kv span:nth-child(odd){color:#6b7280}
</style></head><body><div class="app">
<div class="crumbs"><a href="#">Charges</a> / ch_8812</div>
<h1 class="charge-title">Charge ch_8812 · Acme Coffee</h1>
<div class="meta">$120.00 · captured 12 Nov · card •••• 4242</div>
<div class="actions"><button class="refund-btn">Refund</button><button class="receipt-btn">Receipt</button></div>
<div class="card"><table class="events">
<thead><tr><th>Event</th><th>Amount</th><th>Status</th><th>By</th><th>When</th></tr></thead>
<tbody>
<tr class="charge-row"><td><b>Charge</b></td><td>$120.00</td><td class="status status-captured">captured</td><td>api</td><td>12 Nov, 9:02</td></tr>
<tr class="refund-line"><td class="indent">↳ Partial refund</td><td>−$20.00</td><td class="status status-paid">paid</td><td>dashboard</td><td>13 Nov, 10:15</td></tr>
<tr class="refund-line"><td class="indent">↳ Partial refund</td><td>−$30.00</td><td class="status status-pending">pending</td><td>api</td><td>13 Nov, 7:31</td></tr>
<tr class="balance-row"><td><b>Balance</b></td><td><b>$70.00</b></td><td></td><td></td><td></td></tr>
</tbody></table></div>
<div class="side">
<div class="box customer"><h3>Customer</h3><div class="kv"><span>Name</span><span>Acme Coffee</span><span>Email</span><span>billing@acme.example</span><span>Since</span><span>March 2024</span></div></div>
<div class="box webhooks"><h3>Webhooks</h3><div class="kv"><span>charge.updated</span><span>3 delivered</span><span>refund.updated</span><span>2 delivered · 0 failing</span></div></div>
</div>
<div class="foot">Webhooks · 5 delivered · 0 failing</div>
</div>
<script>history.replaceState(null, "", location.pathname.replace(/\\/$/, "") + "/charges/ch_8812" );</script>
<script src="/preview-overlay.js"></script>
</body></html>`;

export const routes = [
  ["GET", /^\/api\/agents\/([^/]+)\/diff$/, (m) => (agents.some((a) => a.id === m[1]) ? diffOf(m[1]) : [404, { error: "no such agent", error_code: "not_found" }])],
  ["GET", /^\/api\/agents\/([^/]+)\/events$/, (m, url) => {
    const after = Number(url.searchParams.get("after") || 0);
    const limit = Number(url.searchParams.get("limit") || 200);
    const withData = url.searchParams.get("data") === "1";
    return { events: EVENTS.filter((e) => e.agent === m[1] && e.seq > after).slice(0, limit).map(({ data, ...e }) => (withData ? { ...e, data } : e)) };
  }],
  ["PUT", /^\/api\/agents\/([^/]+)\/preview$/, (m, _, body) => {
    const a = agents.find((x) => x.id === m[1]);
    if (!a) return [404, { error: "no such agent", error_code: "not_found" }];
    if (!/^http:\/\/(127\.0\.0\.1|localhost):\d{1,5}(\/.*)?$/.test(String(body?.url || "")))
      return [400, { error: "only http://127.0.0.1:<port> or http://localhost:<port>", error_code: "bad_request" }];
    a.preview_url = body.url;
    return { agent: a };
  }],
  ["POST", /^\/api\/preview\/route$/, (_, __, body) => route(body?.agent, body?.pick)],
];

// Non-JSON routes (levels-mock's `raw` hook): the proxied app and the overlay.
export const raw = [
  ["GET", /^\/preview\/([^/]+)(\/.*)?$/, (m, _url, _req, res) => {
    const a = agents.find((x) => x.id === m[1]);
    if (!a || !a.preview_url) { res.writeHead(404, { "content-type": "text/plain" }); return res.end("no preview for this agent"); }
    res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    res.end(PAGE);
  }],
  ["GET", /^\/preview-overlay\.js$/, (_m, _url, _req, res) => {
    const real = path.resolve(here, "../../preview-overlay.js");
    const file = fs.existsSync(real) ? real : path.join(here, "preview-overlay-stub.js");
    res.writeHead(200, { "content-type": "text/javascript" });
    fs.createReadStream(file).pipe(res);
  }],
];
