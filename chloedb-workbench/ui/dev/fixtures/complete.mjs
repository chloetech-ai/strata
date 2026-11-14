// Autocomplete and intent checks (SPEC.md, "Fifth build: Autocomplete"),
// for the demo repo "shop" of the Trace fixtures (a Rust API in api/, a
// React app in web/): GET /api/complete and POST /api/intent/check, with
// the real routes' shapes. The ranking here is a small stand-in for the
// server's (ChloeDB's PREFIX over complete_items, BM25 over earlier
// tasks); the dev page /dev/complete.html shows every box it serves.
// Past the token, as the server's foresee.rs: proposals for an empty box
// (mode "propose", with the agent's context on a steer), a ghost sentence
// with the ◆ rule that applies and a done check from the covering test, a
// "because" on each row, and POST /api/complete/events and /ghost.
import { NOW } from "./base.mjs";

export const SHOP = "/home/user/src/shop";
const iso = (minAgo) => new Date(NOW - minAgo * 60000).toISOString();
const ago = (min) => (min < 1 ? "just now" : min < 60 ? `${Math.round(min)} min ago` : min < 48 * 60 ? `${Math.round(min / 60)} h ago` : `${Math.round(min / 1440)} days ago`);
const day = (minAgo) => new Date(NOW - minAgo * 60000).toLocaleDateString("en-US", { month: "short", day: "numeric" });
/** What each agent is doing now, as the real `doing_now` says it. */
const NOWS = { a7c1a002: ["working", "Edit api/src/cart/total.rs", 1], a7c1a001: ["working", "Edit web/src/checkout/Summary.tsx", 2], a7c1a000: ["idle", "waiting for its agents", 6], a7c1a003: ["waiting", "Bash cargo test discounts", 9] };
NOWS.a1ed6e12 = ["working", "Edit src/refunds/service.rs", 1];
const doing = (a) => { const [st, what, m] = NOWS[a.id] || [a.state, "starting", 0]; return `${st} · now: ${what} · ${ago(m)}`; };
/** The tests that cover a file, as `covering` finds them. */
const COVER = { "web/src/checkout/Summary.tsx": ["npx vitest run web/src/checkout/Summary.test.tsx", "web/src/checkout/Summary.test.tsx covers web/src/checkout/Summary.tsx"], "api/src/cart/total.rs": ["cargo test --test cart_total", "api/tests/cart_total.rs covers api/src/cart/total.rs"], "api/src/money.rs": ["cargo test money", "the tests of api/src/money.rs"], "web/src/cart/useCart.ts": ["npx vitest run web/src/cart/useCart.test.ts", "web/src/cart/useCart.test.ts covers web/src/cart/useCart.ts"] };

const AGENTS = [
  { id: "a7c1a002", name: "cart", state: "working", running: true, task: "cart_total takes discounts, rounded per line", files: ["api/src/cart/total.rs", "api/src/money.rs"], edited_min: 3 },
  { id: "a7c1a001", name: "checkout", state: "working", running: true, task: "Apply a discount code in the checkout summary", files: ["web/src/checkout/Summary.tsx", "web/src/cart/useCart.ts"], edited_min: 2 },
  { id: "a7c1a000", name: "lead", state: "idle", running: true, task: "Discount codes at checkout, end to end", files: [] },
  { id: "a7c1a003", name: "codes-api", state: "waiting", running: true, task: "POST /discounts/apply validates a code and its limits", files: ["api/src/discounts/code.rs"], edited_min: 9 },
];
const FILES = [
  ["api/src/cart/total.rs", 142], ["api/src/money.rs", 61], ["api/src/cart/mod.rs", 38], ["api/src/discounts/code.rs", 97], ["api/tests/cart_total.rs", 118],
  ["api/migrations/0007_discount_codes.sql", 24], ["web/src/checkout/Summary.tsx", 186], ["web/src/checkout/CodeField.tsx", 74], ["web/src/cart/useCart.ts", 132],
  ["web/src/cart/CartLine.tsx", 66], ["web/src/money/rounding.ts", 28], ["web/src/money/format.ts", 41], ["docs/pricing.md", 52],
];
const SYMBOLS = [
  ["cart_total", "fn", "api/src/cart/total.rs", 18], ["round_half_up", "fn", "api/src/money.rs", 14], ["Cents", "type", "api/src/money.rs", 3],
  ["apply_code", "fn", "api/src/discounts/code.rs", 9], ["roundHalfUp", "function", "web/src/money/rounding.ts", 3], ["formatCents", "function", "web/src/money/format.ts", 12],
  ["useCart", "function", "web/src/cart/useCart.ts", 8], ["Summary", "function", "web/src/checkout/Summary.tsx", 21], ["CodeField", "function", "web/src/checkout/CodeField.tsx", 6],
  ["line_discount", "fn", "api/src/cart/total.rs", 31],
];
const CTX = [
  { id: "c4a7a002:61", kind: "decision", text: "Round each line's discount half up before summing; floor lost a cent on 3 of 40 carts.", by: "cart", ts: iso(26 * 60), files: ["api/src/cart/total.rs"] },
  { id: "r5d1c0a09", kind: "rule", text: "Money rounds half up, per line, in integer cents.", by: "you", ts: iso(26 * 60 + 8), files: [] },
  { id: "c4a7a001:88", kind: "decision", text: "The web total mirrors the API: per-line discounts rounded half up, so the summary never disagrees with the charge.", by: "checkout", ts: iso(25 * 60), files: ["web/src/cart/useCart.ts"] },
  { id: "c4a7a003:7", kind: "preference", text: "Prices are integer cents end to end; never floats.", by: "lead", ts: iso(4 * 1440), files: ["api/src/money.rs"] },
  { id: "c4a7a000:12", kind: "decision", text: "Discount codes are case-insensitive and trimmed; stored upper-case.", by: "lead", ts: iso(23 * 60), files: ["api/src/discounts/code.rs"] },
];
const TESTS = [
  ["api › cart_total::rounds_each_line_half_up", "12 of 12 passed · p50 4 ms · cart", 1],
  ["api › cart_total::three_at_333_with_15_off", "12 of 12 passed · p50 3 ms", 1],
  ["api › discounts::code::is_case_insensitive", "flaky: passed and failed on 4be1f07", 0.8],
  ["web › Summary › shows the discount on each line", "6 of 7 passed · vitest · checkout", 0.86],
  ["web › rounding › roundHalfUp matches the API", "4 of 4 passed · vitest", 1],
];
const TASKS = [
  { agent: "a6f00911", name: "fix-floor", task: "Fix rounding of discounts in cart_total: floor lost a cent on 3 of 40 carts", outcome: "done", detail: "fix-floor · 14 min · Haiku · cargo test cart_total passed" },
  { agent: "a6f00915", name: "web-round", task: "Round the web summary's discounts like the API does", outcome: "done", detail: "web-round · 9 min · Sonnet · vitest rounding passed" },
  { agent: "a6f00920", name: "receipt-pdf", task: "Fix rounding in the receipt PDF's tax line", outcome: "stopped", detail: "receipt-pdf · stopped after 31 min · 2 steers" },
];
const GOALS = [{ id: "g5d1c0a77", text: "Discount codes at checkout, end to end", state: "active" }, { id: "g4c0b1e02", text: "Prices in integer cents everywhere", state: "done" }];
const TABLES = {
  events: ["id", "seq", "ts", "agent", "kind", "tool", "summary", "data"],
  agents: ["id", "name", "repo", "cwd", "branch", "task", "parent", "depth", "state", "state_since", "waiting_for", "model", "created", "ended", "goal", "outcome"],
  test_runs: ["id", "run", "repo", "agent", "commit", "tree", "runner", "command", "test", "outcome", "passed", "ms", "own_ms", "run_ms", "exit_code", "error", "ts"],
  file_edits: ["id", "repo", "agent", "file", "tool", "ts", "seq"],
  token_use: ["id", "repo", "agent", "input", "output", "cache_read", "ts"],
  questions: ["id", "agent", "outcome", "repo", "kind", "text", "state", "answer", "answered_by", "created", "answered"],
  outcomes: ["id", "goal", "title", "state", "agent", "progress", "created", "updated"],
};
const TYPES = { ts: "TIMESTAMP", created: "TIMESTAMP", ended: "TIMESTAMP", answered: "TIMESTAMP", updated: "TIMESTAMP", state_since: "TIMESTAMP", seq: "INT", depth: "INT", ms: "INT", own_ms: "BOOL", run_ms: "INT", exit_code: "INT", input: "INT", output: "INT", cache_read: "INT", progress: "INT", passed: "BOOL", summary: "TEXT", data: "TEXT", task: "TEXT", text: "TEXT", error: "TEXT", command: "TEXT", title: "TEXT", answer: "TEXT", waiting_for: "TEXT" };
const SQL_WORDS = ["SELECT", "FROM", "WHERE", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "COUNT(*)", "COUNT(DISTINCT", "AND", "OR", "NOT", "IN", "AS", "DESC", "JOIN", "ASOF JOIN", "date_bin(", "approx_percentile(", "MATCH(", "bm25("];
const PERSONAS = [["first-timer", "Maya Chen", "a first-time visitor on her phone"], ["keyboard", "Ken Okafor", "a power user who uses only the keyboard"], ["careful", "Ruth Alvarez", "71, careful, reads every label"], ["hurried", "Sam Patel", "in a hurry between meetings"], ["edge-cases", "Ines Duarte", "a QA engineer trying edge cases"]];

/** The word at the cursor: a trigger (@ ◆ #) and what follows. */
function tokenAt(text, cursor) {
  const m = text.slice(0, cursor).match(/[@◆#]?[\p{L}\p{N}_./:\-]*$/u);
  const t = m ? m[0] : "";
  return { text: t, start: cursor - t.length, end: cursor, trigger: /^[@◆#]/.test(t) ? t[0] : null };
}
const words = (s) => s.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean);
const keysOf = (s) => {
  const out = new Set([s.toLowerCase()]);
  for (const w of s.split(/[^\p{L}\p{N}]+/u)) for (const p of w.split(/(?<=[a-z0-9])(?=[A-Z])/)) if (p) out.add(p.toLowerCase());
  for (const w of s.toLowerCase().split(/[\/.]+/)) if (w) out.add(w);
  return [...out];
};
const hit = (keys, p) => (p ? keys.findIndex((k) => k.startsWith(p)) : -1);

const ORDER = {
  task: ["agent", "file", "symbol", "rule", "decision", "preference", "fact", "test", "task"],
  goal: ["task", "goal", "rule", "decision", "agent", "file"],
  steer: ["file", "symbol", "rule", "decision", "test", "agent"],
  answer: ["rule", "decision", "preference", "file", "symbol"],
  trace: ["symbol", "file", "decision", "rule", "agent", "test"],
  quick: ["trace", "symbol", "file", "decision", "rule", "agent", "test", "doc"],
  why: ["decision", "rule", "symbol", "file", "agent"],
  persona: ["persona", "file", "task"],
  change: ["file", "rule", "decision", "agent"],
};

/** What an empty box proposes now (foresee.rs's proposals). */
function proposals(field, agentId) {
  const P = (sub, text, because, extra = {}) => ({ kind: "proposal", sub, label: text, insert: text, detail: sub, because, source: "chloedb", picked: 0, replace: { start: 0, end: 0 }, ref: {}, ...extra });
  if (field === "task" || field === "goal") return [
    P("test", "Fix web › Summary › shows the discount on each line: flaky, failed 3 of 10 runs in the last 4 h; done when `npx vitest run web/src/checkout/Summary.test.tsx` passes 5 times in a row.", "test_runs: last failed 12 min ago (checkout)"),
    P("error", "Fix “TypeError: Cannot read properties of undefined (reading 'cents')” at web/src/checkout/CodeField.tsx:41; done when the preview loads without it.", "runtime error: 14 hits since 38 min ago, in checkout's preview"),
    P("outcome", "Show the applied code and its savings under the total: one line per code, with Remove", "the next step of “Discount codes at checkout, end to end”, not started"),
  ];
  if (field === "steer" && agentId === "a1ed6e12") return [
    // ledger, of the refunds story (fixtures/story.mjs).
    P("said", "Key it on (merchant, client_ref) like charges does, not on the request id.", "you said this to hooks yesterday, when it was at “Edit src/charges/idem.rs”", { picked: 2 }),
    P("test", "refunds::retry_returns_the_first_result fails since your edit of src/refunds/service.rs: fix it before you go on, and run `cargo test retry_returns_the_first_result`.", "test_runs: last failed 4 min ago"),
    P("rule", "Keep to ◆c9e2b114:23: a retried refund returns the first result; it never pays twice.", `ledger is editing src/refunds/service.rs, ◆ decided ${day(30 * 60)} by lead`),
  ];
  if (field === "steer") {
    const a = AGENTS.find((x) => x.id === agentId) || AGENTS[1];
    return [
      P("said", "Don't touch useCart's rounding, call roundHalfUp from web/src/money/rounding.ts instead.", `you said this to cart 2 days ago, when it was at “Edit api/src/money.rs”`, { picked: 2 }),
      P("rule", "Keep to ◆r5d1c0a09: Money rounds half up, per line, in integer cents.", `${a.name} is editing ${a.files[0] || "its files"}, your rule since ${day(26 * 60)}`),
      P("test", "web › Summary › shows the discount on each line fails since your edit of web/src/checkout/Summary.tsx: fix it before you go on, and run `npx vitest run web/src/checkout/Summary.test.tsx`.", "test_runs: last failed 4 min ago"),
    ];
  }
  if (["trace", "quick", "why"].includes(field)) {
    const go = (t) => (field === "quick" ? { go: `#/trace?q=${encodeURIComponent(t)}` } : {});
    const qs = [["Why did cart and checkout change api/src/money.rs today?", "6 edits today, last 3 min ago"], ["When did web › Summary › shows the discount on each line start failing, and what changed then?", "test_runs: last failed 12 min ago (checkout)"], ["Why “cart: round each line's discount half up”, and what did it change?", "commit 4be1f07 by cart, 2 h ago"]];
    return qs.map(([t, b]) => P("question", t, b, go(t)));
  }
  return [];
}

/** The rest of the sentence (foresee.rs's ghost): the word, the ◆ rule that applies, the done check. */
function ghostOf(field, q, tok, top) {
  if (!["task", "goal", "steer"].includes(field) || !q.trim()) return null;
  const parts = [];
  let g = "";
  if (/[\p{L}\p{N}_./]$/u.test(q)) {
    if (!top || !["file", "symbol", "agent", "test"].includes(top.kind)) return null;
    const ins = (top.insert || "").trimEnd();
    if (!ins.toLowerCase().startsWith(tok.text.toLowerCase()) || ins.length <= tok.text.length) return null;
    g = ins.slice(tok.text.length);
    parts.push({ kind: "word", text: g });
  }
  const full = q + g;
  if (words(full).length < 3) return g ? { text: g, parts, because: "completes the word", source: "chloedb" } : null;
  const file = FILES.map((f) => f[0]).find((f) => full.includes(f));
  const rule = /round|discount|total|money|cent/i.test(full) && !full.includes("◆") ? CTX[1] : null;
  const done = ["task", "goal"].includes(field) && !/done when|so that|passes|\btests?\b/i.test(full) && file && COVER[file];
  if (!rule && !done) return g ? { text: g, parts, because: "completes the word", source: "chloedb" } : null;
  const spaced = /\s$/.test(full);
  const because = [];
  if (rule) { const c = spaced ? `per ◆${rule.id}` : `, per ◆${rule.id}`; parts.push({ kind: "rule", text: c, ref: rule }); g += c; because.push("◆ your rule applies"); }
  if (done) { const c = spaced && !rule ? `so that \`${done[0]}\` passes.` : `; done when \`${done[0]}\` passes.`; parts.push({ kind: "done", text: c, command: done[0] }); g += c; because.push(`done check: ${done[1]}`); }
  else if (rule) { parts.push({ kind: "rule", text: "." }); g += "."; }
  return { text: g, parts, because: because.join(" · "), source: "chloedb" };
}

function complete(url) {
  const field = url.searchParams.get("field") || "task";
  const q = url.searchParams.get("q") || "";
  const cursor = Number(url.searchParams.get("cursor") ?? q.length);
  const tok = tokenAt(q, cursor);
  const agentId = url.searchParams.get("agent") || "";
  const user = url.searchParams.get("user") || "you";
  if (!q.trim() && field !== "sql") {
    const a = AGENTS.find((x) => x.id === agentId) || (agentId === "a1ed6e12" ? { id: agentId, name: "ledger" } : null);
    return { field, repo: SHOP, user, mode: "propose", token: { text: "", start: 0, end: 0, trigger: null }, suggestions: proposals(field, agentId), ghost: null,
      context: field === "steer" && a ? { agent: a.name, id: a.id, now: doing(a) } : null, took_ms: 4.1, missing: [] };
  }
  const r = completeTokens(field, q, cursor, tok);
  const a = AGENTS.find((x) => x.id === agentId);
  return { ...r, repo: SHOP, user, mode: "complete", ghost: cursor >= q.length ? ghostOf(field, q, tok, r.suggestions[0]) : null, context: ["steer", "answer"].includes(field) && a ? { agent: a.name, id: a.id, now: doing(a) } : null };
}

function completeTokens(field, q, cursor, tok) {
  const p = tok.text.replace(/^[@◆#]/, "").toLowerCase();
  const rep = { start: tok.start, end: tok.end };
  const out = [];
  if (field === "sql") return { field, token: tok, suggestions: sql(q, cursor, tok), took_ms: 2, missing: [] };
  const want = ORDER[field] || ORDER.task;
  const only = tok.trigger === "@" ? ["agent"] : tok.trigger ? ["decision", "rule", "preference", "fact"] : null;
  const kinds = only || want;
  if (field === "quick" && /^\s*(when|why|how|who)\b|\bchang/i.test(q)) out.push({ kind: "trace", label: `Trace: ${q.trim()}`, detail: "when, why and how it changed, with the evidence", go: `#/trace?q=${encodeURIComponent(q.trim())}`, score: 100 });
  if (!p && !tok.trigger) return { field, token: tok, suggestions: out, took_ms: 1, missing: [] };
  const add = (kind, rank, it) => { if (kinds.includes(kind)) out.push({ kind, replace: rep, ...it, score: (kinds.length - kinds.indexOf(kind)) * 10 + rank }); };
  for (const a of AGENTS) {
    const i = hit([a.name, ...words(a.task)], p);
    if (i >= 0 || tok.trigger === "@") add("agent", i === 0 ? 9 : 2, { label: `@${a.name}`, insert: `@${a.name} `, detail: `${a.state} · ${a.task}`, because: doing(a).replace(/^[^·]+· /, ""), ref: { id: a.id, name: a.name, state: a.state, running: a.running } });
  }
  for (const [path, lines] of FILES) {
    const i = hit(keysOf(path).concat(path.split("/").pop().toLowerCase()), p);
    const by = AGENTS.find((a) => a.files.includes(path));
    if (i >= 0) add("file", 3, { label: path, insert: `${path} `, detail: `${lines} lines`, ...(by ? { because: `${by.name} edited it ${ago(by.edited_min)}` } : {}), ref: { path } });
  }
  for (const [name, kind, path, line] of SYMBOLS) if (hit(keysOf(name), p) >= 0) add("symbol", name.toLowerCase().startsWith(p) ? 6 : 3, { label: name, insert: name, detail: `${kind} · ${path}:${line}`, ref: { path, line, kind } });
  for (const r of CTX) if (tok.trigger || hit(words(r.text), p) >= 0) add(r.kind, 4, { label: r.text, insert: `◆${r.id} `, detail: `${r.by} · ${r.kind === "rule" ? "every agent is told it" : r.files[0] || "shop"}`,
    because: r.kind === "rule" ? `your rule since ${day((NOW - Date.parse(r.ts)) / 60000)}; every agent is told it` : `◆ ${r.kind === "decision" ? "decided" : "noted"} ${day((NOW - Date.parse(r.ts)) / 60000)} by ${r.by}`, ref: { id: r.id, kind: r.kind, text: r.text, by: r.by, ts: r.ts } });
  for (const [name, detail, rate] of TESTS) if (hit(keysOf(name), p) >= 0) add("test", 2, { label: name, insert: name.split(" › ").pop(), detail, because: rate < 1 ? `flaky: failed ${Math.round((1 - rate) * 7)} of 7 runs` : "passed 3 min ago", ref: { test: name, pass_rate: rate } });
  const ws = words(q.slice(0, cursor));
  if (ws.length >= 2 && ["task", "goal"].includes(field)) for (const t of TASKS) {
    const n = ws.filter((w) => w.length > 2 && words(t.task).some((x) => x.startsWith(w))).length;
    if (n >= 2) add("task", n, { label: t.task, insert: t.task, replace: { start: 0, end: q.length }, detail: t.detail, because: `like what you are typing: ${t.name}'s task, 2 days ago`, ref: { agent: t.agent, name: t.name, outcome: t.outcome } });
  }
  if (field === "goal") for (const g of GOALS) if (hit(words(g.text), p) >= 0) add("goal", 1, { label: g.text, insert: g.text, replace: { start: 0, end: q.length }, detail: g.state, ref: { id: g.id, outcome: g.state } });
  for (const [id, name, role] of PERSONAS) if (hit([id, ...words(name), ...words(role)], p) >= 0) add("persona", 2, { label: name, insert: `${name} (${role})`, detail: role, ref: { id } });
  out.sort((a, b) => b.score - a.score);
  return { field, token: tok, suggestions: out.slice(0, 12), took_ms: 3, missing: [] };
}

/** SQL: tables after FROM and JOIN, a table's columns after `t.`, else the
 *  columns of the tables the statement names, then words of the language. */
function sql(q, cursor, tok) {
  const before = q.slice(0, cursor);
  const p = tok.text.toLowerCase();
  const rep = { start: tok.start, end: tok.end };
  const named = [...q.matchAll(/\b(?:from|join)\s+([a-z_]+)(?:\s+(?:as\s+)?([a-z]\w*))?/gi)].map((m) => ({ table: m[1], alias: m[2] }));
  const dot = tok.text.match(/^([a-z_]\w*)\.(\w*)$/i);
  const out = [];
  if (/\b(from|join)\s+[a-z_]*$/i.test(before)) {
    for (const [t, cols] of Object.entries(TABLES)) if (t.startsWith(p)) out.push({ kind: "table", label: t, insert: `${t} `, detail: `${cols.length} columns · ${cols.slice(0, 5).join(", ")}…`, replace: rep, ref: { table: t } });
    return out;
  }
  if (dot) {
    const n = named.find((x) => x.alias === dot[1] || x.table === dot[1]);
    const t = n ? n.table : dot[1];
    for (const c of TABLES[t] || []) if (c.startsWith(dot[2].toLowerCase())) out.push({ kind: "column", label: c, insert: `${dot[1]}.${c}`, detail: `${t} · ${TYPES[c] || "KEYWORD"}`, replace: rep, ref: { table: t, column: c } });
    return out;
  }
  if (!p) return out;
  for (const n of named) for (const c of TABLES[n.table] || []) if (c.startsWith(p)) out.push({ kind: "column", label: c, insert: c, detail: `${n.table} · ${TYPES[c] || "KEYWORD"}`, replace: rep, ref: { table: n.table, column: c } });
  for (const w of SQL_WORDS) if (w.toLowerCase().startsWith(p)) out.push({ kind: w.endsWith("(") ? "function" : "keyword", label: w, insert: w.endsWith("(") ? w : `${w} `, detail: w.endsWith("(") ? "a function of ChloeDB's SQL" : "", replace: rep });
  return out.slice(0, 12);
}

function check(body) {
  const text = (body && body.text) || "";
  const marks = [], refs = [];
  for (const m of text.matchAll(/@([a-z0-9-]+)/g)) {
    const a = AGENTS.find((x) => x.name === m[1]);
    if (a) refs.push({ start: m.index, end: m.index + m[0].length, kind: "agent", label: `@${a.name}`, detail: `${a.state}: ${a.task}` });
    else marks.push({ start: m.index, end: m.index + m[0].length, severity: "error", kind: "unknown_ref", message: `No agent of shop is called ${m[1]}.` });
  }
  for (const m of text.matchAll(/◆([\w:]+)/g)) {
    const r = CTX.find((x) => x.id === m[1]);
    if (r) refs.push({ start: m.index, end: m.index + m[0].length, kind: r.kind, label: r.text, detail: `${r.kind} · ${r.by}` });
  }
  for (const [path] of FILES) {
    let i = text.indexOf(path);
    if (i < 0) continue;
    refs.push({ start: i, end: i + path.length, kind: "file", label: path });
    const by = AGENTS.find((a) => a.running && a.files.includes(path));
    if (by) marks.push({ start: i, end: i + path.length, severity: "warning", kind: "overlap", message: `${by.name} is changing ${path} right now (edited ${by.edited_min} min ago) for “${by.task}”. Two agents on one file collide when they merge.`,
      fix: { label: `Send it to @${by.name} instead`, go: `#/story/${by.id}` }, ref: { kind: "agent", id: by.id, text: by.task, by: by.name } });
  }
  const floor = text.match(/\b(floor|round(?:ed|s)? down|truncat\w*)\b/i);
  if (floor) {
    const rule = CTX[1];
    marks.push({ start: floor.index, end: floor.index + floor[0].length, severity: "error", kind: "contradicts", message: `“${floor[0]}” goes against what shop decided: money rounds half up, per line. The agent would be told the rule at its first prompt and stop to ask you.`,
      ref: { id: rule.id, kind: rule.kind, text: rule.text, by: "you", ts: rule.ts }, fix: { label: "Round half up instead", edits: [{ start: floor.index, end: floor.index + floor[0].length, text: "round half up" }] } });
  }
  const guarded = text.match(/\b(deploy|publish|push to main|migrate)\w*/i);
  if (guarded) marks.push({ start: guarded.index, end: guarded.index + guarded[0].length, severity: "info", kind: "guarded", message: `${guarded[0]} is on shop's guarded list: the agent will propose it with propose_change and wait for you to approve it in Decide.` });
  if (!/\b(test|tests|passes|pass|assert\w*|expect\w*|verify|until|shows?|returns?|done when)\b/i.test(text) && text.trim().split(/\s+/).length >= 3) {
    const end = Math.min(text.length, (text.match(/^[^,.;:!?\n]*/) || [""])[0].length);
    marks.push({ start: 0, end: Math.max(1, end), severity: "warning", kind: "no_done_check", message: "Nothing says how the agent knows it is done: a test that passes, or something you can see. Without one it stops when it thinks it is finished.",
      fix: { label: "Add done check: cargo test cart_total passes", edits: [{ start: text.length, end: text.length, text: `${/[.!?]\s*$/.test(text) ? "" : "."} Done when \`cargo test cart_total\` passes.` }] } });
  }
  marks.sort((a, b) => a.start - b.start);
  return { marks, refs, took_ms: 4, missing: [] };
}

export const routes = [
  ["GET", /^\/api\/complete$/, (_, url) => complete(url)],
  ["POST", /^\/api\/complete\/events$/, (_, __, body) => ({ ok: true, rows: ((body && body.shown) || []).length + (body && body.typed ? 1 : 0), user: (body && body.user) || "you" })],
  // The model ghost: "off" here unless the text asks for it, so the shots show ChloeDB's.
  ["POST", /^\/api\/complete\/ghost$/, (_, __, body) => (/model/i.test((body && body.text) || "") ? { available: true, cached: false, took_ms: 640, ghost: { text: " and say which carts change by a cent.", source: "model", model: "haiku", because: "a fast model, from this box's context" } } : { available: false, why: "the fixture's model is off" })],
  ["POST", /^\/api\/intent\/check$/, (_, __, body) => check(body)],
];
