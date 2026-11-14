// Access and audit: what was changed (who, from where, the object, before and
// after, why), what was refused, and every request the server audited; and
// the API keys, with their owner, last use, expiry, a rotation with a grace
// period, and revoked keys kept in sight.
//
// A server from before kinds (entries without `kind`) answers every admin
// request, reads too: the reads are sorted out here and hidden, and revoked
// keys are found again in its log.

import * as core from "../core.js";
import { h, icon, clear, fmt, card, api, every, toast, modal, seg, empty, table, copy, failure, bus, setToken, menu } from "../core.js";
import { state, refreshWho, isProduction, managed, specName } from "../data.js";
import { tables as catalogTables } from "../catalog.js";

export default {
  id: "access", title: "Access and audit", icon: "access", group: "Change",
  keywords: "keys api key token scopes audit log changes who changed refused refusals security users permissions rotate revoke owner",
  render, onParams,
};

let onChange = null;
function onParams(r) { onChange?.(r.params); }

const can = (scope) => (core.can ? core.can(scope) : true);
const needs = (scope) => (core.needs ? core.needs(scope) : `needs an ${scope} key`);
const DAY = 86400000;

// ---------------------------------------------------------------- times
let TZ = "";
try { TZ = new Intl.DateTimeFormat("en-US", { timeZoneName: "short" }).formatToParts(new Date()).find((p) => p.type === "timeZoneName")?.value || ""; } catch { /* old browser */ }
const sameDay = (a, b) => a.toDateString() === b.toDateString();
const hm = (ms) => new Date(ms).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false });
const whenT = (ms) => (ms ? (core.whenTitle ? core.whenTitle(ms) : new Date(ms).toString()) : "");
/** "Sun, Nov 8, 21:05 UTC". */
const full = (ms) => `${new Date(ms).toLocaleDateString("en-US", { weekday: "short", month: "short", day: "numeric" })}, ${hm(ms)} ${TZ}`.trim();
/** "09:31" today, "Nov 2 09:31" another day. */
function at(ms) {
  if (!ms) return "–";
  const d = new Date(ms);
  return sameDay(d, new Date()) ? hm(ms) : d.toLocaleDateString("en-US", { month: "short", day: "numeric" }) + " " + hm(ms);
}
function dayLabel(ms) {
  const d = new Date(ms), now = new Date();
  const md = d.toLocaleDateString("en-US", { month: "short", day: "numeric", year: d.getFullYear() === now.getFullYear() ? undefined : "numeric" });
  if (sameDay(d, now)) return `Today, ${md}`;
  if (sameDay(d, new Date(Date.now() - DAY))) return `Yesterday, ${md}`;
  return d.toLocaleDateString("en-US", { weekday: "short" }) + ", " + md;
}
function ago(ms) {
  if (!ms) return "never";
  const s = (Date.now() - ms) / 1000;
  if (s < 5) return "just now";
  if (s < 60) return `${Math.floor(s)} s ago`;
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 172800) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} days ago`;
}
function until(ms) {
  const s = (ms - Date.now()) / 1000;
  if (s <= 0) return "expired";
  if (s < 3600) return `in ${Math.max(1, Math.round(s / 60))} min`;
  if (s < 172800) return `in ${Math.round(s / 3600)} h`;
  return `in ${Math.round(s / 86400)} days`;
}
const days = (ms) => Math.floor(ms / DAY);

// ---------------------------------------------------------------- entries
const AGENTS = "@agents";
const AUTO = { operator: "the operator", autoscaler: "the autoscaler", upgrader: "the upgrader" };
const isToken = (w) => w === "token" || w === "root token";
const auto = (e) => !!(e.actor && AUTO[e.actor]);
/** Who did it, in words. */
function whoName(e) {
  if (auto(e)) return AUTO[e.actor];
  if (isToken(e.who)) return "the server token";
  if (e.who === "cluster") return "another server";
  if (e.who === "open") return "anyone";
  if (!e.who || e.who === "unknown") return "no valid key";
  return e.who;
}
const clientName = (c) => (!c ? null : c === "command-center" ? "UI" : c);
// Each key's role (ops, propose) by its id, from the keys listed: an
// audit entry says the scope a role key is kept as (read), not its role.
const roles = new Map();
/** An agent's entry, in words: "agent (ops) for dana". */
const agentText = (e) => `agent${roles.get(e.key) ? ` (${roles.get(e.key)})` : ""}${e.on_behalf_of ? ` for ${e.on_behalf_of}` : ""}`;
/** How: "admin key · UI · 10.0.0.4", "agent (ops) for dana · curl", "automatic". */
function whoSub(e) {
  if (auto(e)) return "automatic";
  const parts = [];
  if (isToken(e.who)) parts.push(e.actor === "agent" ? `${agentText(e)}, shared --token` : "shared, --token");
  else if (e.who === "cluster") parts.push("the cluster's token");
  else if (e.actor === "agent") parts.push(agentText(e));
  else if (e.who && e.who !== "unknown" && e.who !== "open") parts.push(e.scope ? `${e.scope} key` : "key");
  const c = clientName(e.client);
  if (c) parts.push(c);
  if (e.addr) parts.push(e.addr);
  // R2-data-tools-4: with several coordinators, the one that kept it.
  if (e.coordinator && manyCoords) parts.push(`via ${e.coordinator}`);
  return parts.join(" · ");
}

// Whether the audit came from more than one coordinator (set at each load).
let manyCoords = false;
// POSTs that ask without changing anything.
const PASSIVE = /\/(plan|check|validate|replay)(\/|$)/;
/** "change", "refusal", "note" or "read" (a server from before kinds audits reads). */
function sortOf(e) {
  // A read the server answered with an error is no change, whatever its kind says.
  if (e.kind === "change" && (e.method === "GET" || e.method === "HEAD")) return "note";
  if (e.kind) return e.kind;
  if (e.status === 401 || e.status === 403) return "refusal";
  const read = e.method === "GET" || e.method === "HEAD" || (e.method === "POST" && PASSIVE.test(e.route || ""));
  if (read) return e.detail ? "note" : "read";
  return "change";
}

const AREAS = [["topology", "topology"], ["rollout", "rollouts"], ["autoscale", "autoscaling"], ["keys", "keys"], ["settings", "settings"], ["data", "data"], ["alerts", "alerts"], ["storage", "storage"], ["cloud", "cloud"], ["queries", "queries"], ["other", "other"]];
const AREA = Object.fromEntries(AREAS);
/** What part of the cluster an entry is about. */
function area(e) {
  const r = e.route || "", o = String(e.object || "").toLowerCase();
  if (/^key /.test(o) || r.startsWith("/v1/admin/keys")) return "keys";
  if (r.includes("/autoscale") || e.actor === "autoscaler") return "autoscale";
  if (r.startsWith("/v1/admin/kube") || e.actor === "upgrader") return "rollout";
  if (/^\/v1\/admin\/(shards|moves|rebalance|promote|slots)/.test(r) || /^(shard|copy) /.test(o) || e.actor === "operator") return "topology";
  if (r.startsWith("/v1/alerts") || /^alert/.test(o)) return "alerts";
  if (/^\/v1\/admin\/(checkpoint|vacuum|verify)/.test(r)) return "storage";
  if (/^\/v1\/admin\/(aws|digitalocean|cloud)/.test(r)) return "cloud";
  if (r.startsWith("/v1/queries")) return "queries";
  if (/^\/v1\/(sql|write|import|stream|kv|index|synonyms|export|query)/.test(r) || /^table /.test(o)) return "data";
  if (/^\/v1\/(admin\/)?(log|logs|profile|settings|system)/.test(r) || /^setting/.test(o)) return "settings";
  return "other";
}

const HEX40 = /^[0-9a-f]{40}/;
function shortImage(ref) {
  const i = ref.indexOf("@sha256:");
  if (i > 0) return ref.slice(i + 8, i + 8 + 12);
  const tag = ref.slice(ref.lastIndexOf(":") + 1);
  return HEX40.test(tag) ? tag.slice(0, 7) : tag;
}
const SQL = /^\s*(select|insert|update|delete|create|drop|alter|with|truncate|replace)\b/i;
// [route, done, failed]
const ROUTE_TITLES = [
  // AC-3: a plan's own calls, refused by its checks or not.
  [/^\/v1\/admin\/plan\/apply/, "Applied a cluster plan", "A cluster plan was refused"],
  [/^\/v1\/admin\/plan\/run\/stop/, "Stopped the running plan", "Stopping the plan failed"],
  [/^\/v1\/admin\/plan\/run\/resume/, "Resumed the plan", "Resuming the plan failed"],
  [/^\/v1\/admin\/plan\/run\/discard/, "Discarded the stopped plan", "Discarding the plan failed"],
  [/^\/v1\/admin\/plan$/, "Checked a cluster plan", "A cluster plan's checks failed"],
  [/^\/v1\/admin\/kube$/, "Changed the StrataCluster", "A change to the StrataCluster failed"],
  [/^\/v1\/admin\/kube\/resume/, "Resumed the rollout", "Resuming the rollout failed"],
  [/^\/v1\/admin\/kube\/upgrade/, "Started an upgrade", "An upgrade failed to start"],
  [/^\/v1\/admin\/kube\/autoscale\/dismiss/, "Dismissed the autoscaling suggestion", "Dismissing the autoscaling suggestion failed"],
  [/^\/v1\/admin\/checkpoint/, "Checkpointed", "A checkpoint failed"],
  [/^\/v1\/admin\/vacuum/, "Vacuumed", "A vacuum failed"],
  [/^\/v1\/admin\/verify/, "Verified the files", "Verifying the files failed"],
  [/^\/v1\/admin\/promote/, "Promoted a copy", "A promotion failed"],
  [/^\/v1\/admin\/rebalance/, "Rebalanced the slots", "A rebalance failed"],
  [/^\/v1\/alerts\/rules/, "Changed the alert rules", "A change to the alert rules failed"],
  [/^\/v1\/admin\/keys$/, "Created a key", "Creating a key failed"],
  [/^\/v1\/admin\/keys\/:id$/, "Revoked a key", "Revoking a key failed"],
  [/^\/v1\/(sql|query)/, "Ran SQL", "A statement failed"],
];

// ---------------------------------------------------------------- before and after
/** Flattens a value to its leaves: {"a.b": "1"}; lists of named objects go by name. */
function flat(v, p = "", out = {}) {
  if (Array.isArray(v) && v.length && v.every((x) => x && typeof x === "object" && (x.name != null || x.id != null))) {
    for (const x of v) flat(x, `${p}[${x.name ?? x.id}]`, out);
  } else if (v && typeof v === "object" && !Array.isArray(v)) {
    const ks = Object.keys(v);
    if (!ks.length && p) out[p] = "{}";
    for (const k of ks) flat(v[k], p ? `${p}.${k}` : k, out);
  } else out[p || "value"] = JSON.stringify(v);
  return out;
}
/** What differs: [[path, before, after]] (undefined: absent). */
function differs(b, a) {
  const x = b === undefined ? {} : flat(b), y = a === undefined ? {} : flat(a);
  return [...new Set([...Object.keys(x), ...Object.keys(y)])].filter((k) => x[k] !== y[k]).map((k) => [k, x[k], y[k]]);
}
const clip = (s, n = 80) => { const v = /^"[^"]*"$/.test(s) ? s.slice(1, -1) : s; return v.length > n ? v.slice(0, n - 1) + "…" : v; };
/** A field's path as people read it: `expires_ms` is "expires", `for_s` is "for". */
const said = (p) => p.replace(/_(ms|s)\b/g, "");
/** A leaf as people read it: a time for `*_ms` epochs, the rest as it is. */
// A value said as a person would: no expiry is "never", nothing set is "none".
const leaf = (path, v) => (v === "null" ? (/expires/.test(path) ? "never" : "none") : /_ms$/.test(path) && /^\d{12,}$/.test(v || "") ? `${at(+v)} ${TZ}`.trim() : /_s$/.test(path) && /^\d+(\.\d+)?$/.test(v || "") ? fmt.ms(+v * 1000) : clip(v));

// ---------------------------------------------------------------- dialogs
/** Asks before a change: resolves false or {reason}; Cancel has the focus. */
function ask({ title, body, ok = "Confirm", danger = false, reason = null, wide = true }) {
  // On production every change says why, as Cluster's Apply asks (NEW-cluster-admin-13).
  if (reason === "optional" && isProduction()) reason = "required";
  return new Promise((resolve) => {
    let answered = false;
    const yes = h("button", { class: "btn " + (danger ? "danger solid" : "primary") }, ok);
    const no = h("button", { class: "btn" }, "Cancel");
    const why = reason ? h("textarea", { class: "input ro-why", rows: 2, placeholder: reason === "required" ? "Why, for the audit log (required)" : "Why, for the audit log (optional)" }) : null;
    const sync = () => { yes.disabled = reason === "required" && !why.value.trim(); };
    if (why) { why.oninput = sync; sync(); }
    const m = modal({
      title, wide,
      body: h("div", { class: "stack ro-dlg" }, body, why ? h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, reason === "required" ? "Reason" : "Reason (optional)"), why) : null),
      foot: [no, yes], onClose: () => { if (!answered) resolve(false); },
    });
    yes.onclick = () => { answered = true; m.close(); resolve({ reason: why?.value.trim() || "" }); };
    no.onclick = () => { answered = true; m.close(); resolve(false); };
    setTimeout(() => no.focus(), 0);
  });
}
const facts = (rows) => h("dl", { class: "ro-facts" }, rows.filter(Boolean).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]));
/** An answer from a server without this route (axum's empty 404/405), not ChloeDB's error. */
const missing = (e) => (e?.status === 404 || e?.status === 405) && !e?.body?.error;

const SCOPES = [
  ["feed", "Feed", "the feed alone, for another cluster to follow this one, and health"],
  ["read", "Read", "queries, search, schema, stats, the series and alerts, and the feed"],
  ["write", "Write", "also writes, imports, streams, the agent, INSERT, UPDATE and DELETE"],
  ["admin", "Admin", "also DDL, keys, the audit log, alert rules, checkpoints, vacuum, export, others' queries and the logs"],
];
// Keys for agents (#482): a role, or another scope marked as an agent's.
const AGENT_SCOPES = [
  ["propose", "Propose", "reads, the server's views and the audit log; files change requests for a person to approve"],
  ["ops", "Ops", "also acts alone on bounded actions, with a reason: acknowledge, silence and note alerts, cancel a query, contain a key, checkpoint, scrub, verify"],
  ["read", "Read", "reads the tables, marked as an agent's"],
  ["admin", "Admin", "every scope, marked as an agent's: it still says why, and takes no tier 3 action alone"],
];
const MATRIX = [
  ["Read the feed (a cluster following this one)", 1, 1, 1, 1],
  ["Run queries, search, explain", 0, 1, 1, 1],
  ["Read the schema, stats, series, alerts, cluster", 0, 1, 1, 1],
  ["Write rows, import files, stream", 0, 0, 1, 1],
  ["INSERT, UPDATE, DELETE", 0, 0, 1, 1],
  ["Ask the agent", 0, 0, 1, 1],
  ["CREATE, DROP (DDL), over HTTP and gRPC", 0, 0, 0, 1],
  ["See and cancel others' queries; read the logs", 0, 0, 0, 1],
  ["Checkpoint, vacuum, verify, promote, roll out", 0, 0, 0, 1],
  ["Export the database", 0, 0, 0, 1],
  ["Manage keys and alert rules; read the audit log", 0, 0, 0, 1],
];
const SUCCESSOR = /^(\d{4}-\d{2}(-\d+)?|v?\d+|new)$/;

// ---------------------------------------------------------------- the view
const TABS = ["changes", "refused", "all"];

function render(page, r) {
  const pick = (t) => (TABS.includes(t) ? t : "changes");
  let tab = pick(r.params.get("tab"));
  const f = { who: r.params.get("who") || "", area: r.params.get("kind") || "", q: r.params.get("q") || "" };
  let win = { from: +r.params.get("from") || null, to: +r.params.get("to") || null };

  let audit = null, auditErr = null;   // {changes, refusals, reads, notes, file, kinds}
  let everything = null;              // the full log (a newer server: fetched on its tab)
  let keysJ = null, keysErr = null;   // GET /v1/admin/keys
  let rotateMissing = false;          // the server answers no /rotate
  let shown = 200;
  const opened = new Set();

  const head = h("div"), chipsBox = h("div", { class: "ac-chips" });
  const listBox = h("div"), keysBox = h("div"), rotBox = h("div", { class: "stack", style: { gap: "14px" } });
  const tabsBox = h("span"), whoSel = h("select", { class: "select sm", "aria-label": "Who" }), areaSel = h("select", { class: "select sm", "aria-label": "Kind of change" });
  const search = h("input", { class: "input sm ac-q", type: "search", placeholder: "Shard, copy, key, table or address…", value: f.q, "aria-label": "Search" });
  const keptLine = h("div", { class: "small muted ac-kept" });
  const bannerBox = h("div");
  const hiddenBox = h("span", { class: "small muted ac-hidden" });
  const changesCard = h("section", { class: "card ac-chg" },
    h("div", { class: "hd" }, h("h3", {}, "Changes"), h("span", { class: "sub ac-chg-sub" }), hiddenBox),
    h("div", { class: "bd" }, h("div", { class: "row ac-filters" }, tabsBox, whoSel, areaSel, search), keptLine, bannerBox, listBox));
  const keysCard = h("section", { class: "card ac-keys" }, h("div", { class: "hd" }, h("h3", {}, "Keys"), h("span", { class: "sub ac-keys-sub" })), h("div", { class: "bd flush" }, keysBox));
  page.append(head, chipsBox, h("div", { class: "ac-cols" }, changesCard, h("div", { class: "stack", style: { gap: "14px", minWidth: 0 } }, keysCard, rotBox, scopesCard())));

  whoSel.onchange = () => { f.who = whoSel.value; setParams(); drawList(); };
  areaSel.onchange = () => { f.area = areaSel.value; setParams(); drawList(); };
  let typing = null;
  search.oninput = () => { clearTimeout(typing); typing = setTimeout(() => { f.q = search.value.trim(); setParams(); shown = 200; drawList(); }, 150); };

  function setParams() {
    const q = new URLSearchParams(location.hash.split("?")[1] || "");
    const put = (k, v) => (v ? q.set(k, v) : q.delete(k));
    put("tab", tab === "changes" ? "" : tab); put("who", f.who); put("kind", f.area); put("q", f.q);
    const want = "#/access" + (q.toString() ? "?" + q : "");
    if (location.hash !== want) window.history.replaceState(null, "", want);
  }
  function setTab(t) { tab = pick(t); shown = 200; setParams(); if (tab === "all" && audit?.kinds && !everything) loadAll(); drawList(); }
  onChange = (p) => {
    const t = pick(p.get("tab")), w = p.get("who") || "", a = p.get("kind") || "", q = p.get("q") || "";
    if (t === tab && w === f.who && a === f.area && q === f.q) return;
    tab = t; f.who = w; f.area = a; f.q = q; search.value = q;
    win = { from: +p.get("from") || null, to: +p.get("to") || null };
    if (p.get("new")) create();
    draw();
  };

  // ---------------------------------------------------------- loading
  const inWin = (e) => (!win.from || e.ts_ms >= win.from) && (!win.to || e.ts_ms <= win.to);
  async function loadAudit() {
    const qs = (kind, limit) => "/v1/admin/audit?" + new URLSearchParams({ ...(kind ? { kind } : {}), limit: String(limit), ...(win.from ? { from: String(win.from) } : {}), ...(win.to ? { to: String(win.to) } : {}) });
    try {
      const j = await api(qs("change", 2000));
      const es = (j.entries || []).filter(inWin);
      const kinds = es.some((e) => e.kind) || (audit?.kinds ?? false);
      if (kinds) {
        // A newer server sorts them: changes here, refusals asked apart.
        let refusals = audit?.refusals || [];
        try { refusals = ((await api(qs("refusal", 500))).entries || []).filter(inWin); } catch { /* keep the last */ }
        audit = { kinds: true, changes: es.filter((e) => sortOf(e) === "change"), refusals, reads: 0, notes: [], file: j.file, coords: j.coordinators || null };
        if (tab === "all") loadAll();
      } else {
        const by = { change: [], refusal: [], note: [], read: [] };
        for (const e of es) (by[sortOf(e)] || by.change).push(e);
        audit = { kinds: false, changes: by.change, refusals: by.refusal, reads: by.read.length, notes: by.note, file: j.file, all: es, coords: j.coordinators || null };
      }
      manyCoords = (audit.coords?.length || 0) > 1;
      auditErr = null;
    } catch (e) { auditErr = e; }
  }
  async function loadAll() {
    try { everything = ((await api("/v1/admin/audit?limit=2000")).entries || []).filter(inWin); }
    catch { everything = null; }
    if (tab === "all") drawList();
  }
  async function loadKeys() {
    try { keysJ = await api("/v1/admin/keys"); keysErr = null; for (const k of keysJ.keys || []) if (k.scope === "ops" || k.scope === "propose") roles.set(k.id, k.scope); }
    catch (e) { keysErr = e; }
  }
  // The clients that send each key (AC-4): every query of the last 24 h
  // through every coordinator (/v1/admin/keys/clients), and the audited
  // changes, by key id. An older server: the last queries it answered.
  let callers = null;
  async function loadCallers() {
    try {
      const j = await api("/v1/admin/keys/clients?hours=24");
      callers = { keys: j.keys || {}, since: j.since_ms || null, coords: j.coordinators || null, at: Date.now() };
    } catch {
      try { const j = await api("/v1/queries/recent?limit=500"); callers = { qs: j.queries || [], at: Date.now() }; }
      catch { callers = callers || { qs: [], at: Date.now(), none: true }; }
    }
  }
  /** [{label, addr, n, last}] for key `id`, most used first; `since`: the oldest query read. */
  function clientsOf(id) {
    const by = new Map();
    const add = (c, ts, what) => {
      const label = c.app || (c.client === "command-center" ? "the command center" : c.client) || "a client";
      const k = `${label}|${c.addr || ""}`;
      const x = by.get(k) || { label, addr: c.addr || null, queries: 0, changes: 0, last: 0 };
      x[what]++; x.last = Math.max(x.last, ts || 0);
      by.set(k, x);
    };
    for (const q of callers?.qs || []) if (q.key_id === id) add(q, q.ts_ms, "queries");
    for (const c of callers?.keys?.[id] || []) {
      const label = c.app || (c.client === "command-center" ? "the command center" : c.client) || "a client";
      const k = `${label}|${c.addr || ""}`;
      const x = by.get(k) || { label, addr: c.addr || null, queries: 0, changes: 0, last: 0, failed: 0, via: [] };
      x.queries += c.calls || 0; x.failed = (x.failed || 0) + (c.failed || 0); x.last = Math.max(x.last, c.last_ms || 0);
      x.via = [...new Set([...(x.via || []), ...(c.coordinators || [])])];
      by.set(k, x);
    }
    for (const e of [...(audit?.changes || []), ...(audit?.refusals || [])]) if (e.key === id) add({ app: e.client && e.client !== "command-center" ? e.client : null, client: e.client, addr: e.addr }, e.ts_ms, "changes");
    return [...by.values()].sort((a, b) => b.queries + b.changes - (a.queries + a.changes) || b.last - a.last);
  }
  const callersSince = () => { if (callers?.keys) return callers.since; const ts = (callers?.qs || []).map((q) => q.ts_ms).filter(Boolean); return ts.length ? Math.min(...ts) : null; };
  /** What the key's clients were read from, in words, for the clients' tooltip. */
  const callersFrom = () => {
    const since = callersSince();
    if (!callers?.keys) return `the last ${fmt.n(callers?.qs?.length || 0)} queries this server answered${since ? ` (since ${whenT(since)})` : ""}`;
    const cs = callers.coords || [], down = cs.filter((c) => !c.ok);
    return `every query of the last 24 h${since && since > Date.now() - 24 * 3600e3 + 60e3 ? ` (counted since ${whenT(since)}, when a coordinator started)` : ""}${cs.length > 1 ? `, through ${down.length ? `${cs.length - down.length} of ${cs.length} coordinators (${down.map((c) => c.name).join(", ")} did not answer)` : `all ${cs.length} coordinators`}` : ""}`;
  };
  const clientSaid = (x) => `${x.label}${x.addr ? ` from ${x.addr}` : ""} (${[x.queries ? fmt.plural(x.queries, "query", "queries") : null, x.failed ? `${fmt.n(x.failed)} failed` : null, x.changes ? fmt.plural(x.changes, "audited request") : null, x.via?.length > 1 || (x.via?.length && (callers?.coords || []).length > 1) ? `via ${x.via.join(", ")}` : null].filter(Boolean).join(", ")}, last ${ago(x.last)})`;
  async function load() {
    await Promise.all([loadAudit(), loadKeys(), loadCallers()]);
    draw();
  }

  // ---------------------------------------------------------- keys, worked out
  const now = () => Date.now();
  /** Key ids to names, from the keys and the log's "created key NAME (ID, SCOPE)". */
  function names() {
    const m = {};
    for (const e of audit?.changes || []) {
      const x = /created key (.+?) \(([0-9a-f]{8,}), (feed|read|write|admin)\)/.exec(e.detail || "");
      if (x) m[x[2]] = x[1];
    }
    for (const k of keysJ?.keys || []) m[k.id] = k.name;
    return m;
  }
  /** Revoked keys: listed by a newer server; found in the log otherwise. */
  function revokedKeys() {
    const listed = (keysJ?.keys || []).filter((k) => k.revoked_ms);
    if (listed.length || !audit || audit.kinds) return listed;
    const made = {}, gone = [];
    for (const e of audit.changes) {
      const c = /created key (.+?) \(([0-9a-f]{8,}), (feed|read|write|admin)\)/.exec(e.detail || "");
      if (c && e.status < 400) made[c[2]] = { name: c[1], scope: c[3], created_ms: e.ts_ms, created_by: whoName(e) };
      const v = /revoked key ([0-9a-f]{8,})/.exec(e.detail || "");
      if (v && e.status < 400) gone.push({ id: v[1], revoked_ms: e.ts_ms, revoked_by: e.who });
    }
    const live = new Set((keysJ?.keys || []).map((k) => k.id));
    return gone.filter((g) => !live.has(g.id)).map((g) => ({ ...made[g.id], ...g, name: made[g.id]?.name || `key ${g.id.slice(0, 8)}`, fromLog: true }));
  }
  /** Whether a key is the one an audit entry names: by id when the server says it, else by name. */
  const keyOf = (e) => (k) => (e.key ? k.id === e.key : k.name === e.who);
  const liveKeys = () => (keysJ?.keys || []).filter((k) => !k.revoked_ms);
  /** Rotations under way: [{old, next}] (a newer key of the same scope that succeeds it). */
  function rotations() {
    const live = liveKeys(), out = [];
    for (const k of live) {
      const next = live.filter((s) => s.id !== k.id && s.created_ms > k.created_ms && (s.scope === k.scope) &&
        (s.rotated_from === k.id || s.successor_of === k.id || k.successor === s.id || s.name === k.name ||
          (s.name.startsWith(k.name + "-") && SUCCESSOR.test(s.name.slice(k.name.length + 1)))))
        .sort((a, b) => b.created_ms - a.created_ms)[0];
      if (next) out.push({ old: k, next });
    }
    return out;
  }
  const unusedAdmin = (k) => k.scope === "admin" && !k.revoked_ms && ((k.last_used_ms && now() - k.last_used_ms > 30 * DAY) || (!k.last_used_ms && now() - k.created_ms > 7 * DAY));
  // An agent's key lasts one task by design: its short life is no warning.
  const expiringSoon = (k) => !k.revoked_ms && !k.agent && k.expires_ms && k.expires_ms - now() < 14 * DAY;
  const tokenUsed = () => Math.max(0, ...[...(audit?.all || audit?.changes || []), ...(audit?.refusals || [])].filter((e) => isToken(e.who) && e.status < 400).map((e) => e.ts_ms));

  // ---------------------------------------------------------- drawing
  function draw() {
    drawHead();
    drawChips();
    drawTabs();
    drawList();
    drawKeys();
    drawRotations();
  }

  function drawHead() {
    const w = state.whoami;
    const you = !w ? "not signed in" : w.root ? [h("b", {}, "the server token"), " (shared)"] : [h("b", {}, w.name), ` (${w.scope} key)`];
    const exportBtn = h("button", { class: "btn", title: "The entries shown, with your filters, as a CSV file", onclick: exportCsv }, icon("download"), "Export as CSV");
    const createBtn = h("button", { class: "btn primary", onclick: () => create() }, icon("plus"), "Create a key");
    if (core.gate) core.gate(createBtn, "admin"); else if (!can("admin")) createBtn.disabled = true;
    clear(head, h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, "Access and audit"),
        h("p", {}, "Who may change this cluster, who did, and the keys they use · signed in as ", ...[you].flat())),
      h("div", { class: "tools" }, exportBtn, createBtn)));
  }

  function drawChips() {
    const a = state.whoami?.auth || {};
    const live = liveKeys(), out = [];
    const open = !!a.open && keysJ && !live.length;
    if (keysJ || state.whoami) out.push(open ? ["bad", "anyone who reaches this server may do anything", "No token and no keys: create an admin key to close it."]
      : ["ok", "every request needs a key", a.token ? "The server token (--token) or an API key." : "An API key."]);
    out.push(location.protocol === "https:" ? ["ok", "TLS", "This page came over TLS: keys travel encrypted."]
      : /^(127\.0\.0\.1|localhost|\[::1\])$/.test(location.hostname) ? ["ok", "this machine only", "Served on this machine: use TLS (--tls-cert, --tls-key) before serving others."]
        : ["warn", "plain HTTP", "Keys cross the network in the clear: start the server with --tls-cert and --tls-key."]);
    if (keysJ && !live.length && !open) out.push(["warn", "no API keys: every client shares the server token", "Give each client its own key, with the least scope it needs.", () => create()]);
    const rots = rotations();
    for (const k of live.filter(expiringSoon).slice(0, 3)) {
      const rot = rots.find((x) => x.old.id === k.id);
      out.push([k.expires_ms < now() ? "bad" : "warn", `${k.name} ${k.expires_ms < now() ? "expired" : "expires " + until(k.expires_ms)}${rot ? ", rotation under way" : ""}`, `Expires ${whenT(k.expires_ms)}`, () => focusKey(k.id)]);
    }
    for (const k of live.filter(unusedAdmin).slice(0, 2)) {
      out.push(["warn", `${k.name}: admin, ${k.last_used_ms ? `unused for ${days(now() - k.last_used_ms)} days` : "never used"}${k.expires_ms ? "" : ", never expires"}`, "An admin key nobody uses: revoke it, or give it an owner and an expiry.", () => focusKey(k.id)]);
    }
    if (audit && a.token) {
      const week = audit.changes.filter((e) => e.ts_ms > now() - 7 * DAY && !auto(e));
      const n = week.filter((e) => isToken(e.who)).length;
      if (n) out.push(["warn", `the shared server token made ${n} of ${week.length} changes in 7 days`, "A change made with the token names no person: give each person a key of their own.", () => { f.who = "the server token"; tab = "changes"; setParams(); draw(); }]);
    }
    clear(chipsBox, out.map(([lv, text, title, go]) => h(go ? "button" : "span", { class: "ac-chip " + lv, title, type: go ? "button" : null, onclick: go || null },
      icon(lv === "ok" ? "check" : lv === "bad" ? "bad" : "warn"), text)));
  }

  function drawTabs() {
    const nRef = audit ? audit.refusals.length : null;
    clear(tabsBox, seg([["changes", "Changes"], ["refused", nRef ? `Refused · ${fmt.n(nRef)}` : "Refused"], ["all", "Every request"]], tab, setTab, { "aria-label": "Which entries" }));
    // Who and what, from what is loaded.
    const es = [...(audit?.changes || []), ...(audit?.refusals || [])];
    const whos = [...new Set(es.map(whoName))].sort((x, y) => (x.startsWith("the ") ? 1 : 0) - (y.startsWith("the ") ? 1 : 0) || x.localeCompare(y));
    if (f.who && f.who !== AGENTS && !whos.includes(f.who)) whos.push(f.who);
    const agents = es.some((e) => e.actor === "agent") || f.who === AGENTS;
    // A list being picked from is left as it is.
    if (document.activeElement !== whoSel) {
      clear(whoSel, h("option", { value: "" }, "anyone"), agents ? h("option", { value: AGENTS }, "agents") : null, whos.map((w) => h("option", { value: w }, w)));
      whoSel.value = f.who;
    }
    const present = new Set(es.map(area));
    if (document.activeElement !== areaSel) {
      clear(areaSel, h("option", { value: "" }, "any kind"), AREAS.filter(([k]) => present.has(k) || k === f.area).map(([k, l]) => h("option", { value: k }, l)));
      areaSel.value = f.area;
    }
    if (document.activeElement !== search && search.value !== f.q) search.value = f.q;
    // How much is kept, and where.
    const sub = changesCard.querySelector(".ac-chg-sub");
    const n24 = (audit?.changes || []).filter((e) => e.ts_ms > now() - DAY).length;
    sub.textContent = audit ? `what changed the cluster, its data or its access · ${fmt.n(n24)} in 24 h` : "";
    if (audit && !audit.kinds && audit.reads) {
      const all = audit.all.length;
      clear(hiddenBox, `${fmt.n(audit.reads)} read requests hidden (${Math.round(100 * audit.reads / Math.max(1, all))}% of what this server recorded, pages polling among them) · `,
        h("a", { href: "#", onclick: (e) => { e.preventDefault(); setTab("all"); drawTabs(); } }, "show every request"));
    } else clear(hiddenBox);
    const cs = audit?.coords || [], missing = cs.filter((c) => !c.ok);
    clear(keptLine, !audit ? "" : [
      cs.length > 1 ? (missing.length
        ? h("b", { style: { color: "var(--warn)" } }, `${cs.length - missing.length} of ${cs.length} coordinators answered: ${missing.map((c) => c.name || c.url).join(", ")} did not, so what went through ${missing.length === 1 ? "it" : "them"} is missing here · `)
        : `Every coordinator's log (${cs.length}) · `) : null,
      audit.file ? `Written to ${audit.file} on the server` : managed() ? `Kept in memory only: give each coordinator a volume (spec.coordinatorStorage in the StrataCluster ${specName()}) to keep it` : "Kept in memory only: start the server with --audit-log FILE to keep it",
      audit.kinds ? " · the latest 20,000 changes here" : ` · this server keeps its latest 2,000 requests here, reads among them`,
      win.from || win.to ? [" · only ", core.span ? core.span(win.from || 0, win.to || now()) : `${at(win.from)} – ${at(win.to)}`, " · ", h("a", { href: "#", onclick: (e) => { e.preventDefault(); win = { from: null, to: null }; setParams(); load(); } }, "show all")] : null,
    ]);
    drawBanner();
  }

  function drawBanner() {
    clear(bannerBox);
    if (!audit) return;
    const since = now() - DAY, groups = new Map();
    for (const e of audit.refusals) {
      if (e.ts_ms < since || e.client === "command-center") continue;
      const k = `${e.key || e.who}|${e.addr || ""}|${e.client || ""}|${e.status}`;
      const g = groups.get(k) || { n: 0, first: e.ts_ms, last: e.ts_ms, e };
      g.n++; g.first = Math.min(g.first, e.ts_ms);
      if (e.ts_ms > g.last) { g.last = e.ts_ms; g.e = e; }
      groups.set(k, g);
    }
    // The client refused most recently, among those refused three times or more.
    const top = [...groups.values()].filter((g) => g.n >= 3).sort((a, b) => b.last - a.last)[0];
    if (!top || top.n < 3) return;
    const e = top.e, from = [e.addr, clientName(e.client)].filter(Boolean);
    const revoked = revokedKeys().find(keyOf(e));
    const expired = !revoked && (/\bexpired\b/.test(e.detail || "") || liveKeys().find(keyOf(e))?.expires_ms < e.ts_ms);
    const what = revoked || expired ? [`${top.n} requests refused with the ${revoked ? "revoked" : "expired"} key `, h("b", {}, e.who), ` since ${at(top.first)}`]
      : e.status === 403 && e.actor === "agent" ? [`${top.n} requests from `, h("b", {}, whoName(e)), ` (${agentText(e)}) refused since ${at(top.first)}: beyond what the agent may do alone; it files a change request for a person to approve`]
      : e.status === 403 ? [`${top.n} requests from `, h("b", {}, whoName(e)), ` refused since ${at(top.first)}: they need a wider scope`]
      : e.status !== 401 && e.after?.error ? [`${top.n} requests from `, h("b", {}, whoName(e)), ` refused since ${at(top.first)}: ${/x-chloedb-reason|says why/.test(e.after.error) ? "they did not say why (an agent's change needs a reason)" : /at most/.test(e.after.error) ? "they asked past an agent's limit" : e.after.error}`]
        : [`${top.n} requests refused since ${at(top.first)}: no key, or a wrong, expired or revoked one`];
    clear(bannerBox, h("div", { class: "ac-banner" },
      h("span", { class: "ac-dot" }),
      h("div", { class: "grow" }, ...what, from.length ? `, from ${from[0]}${from[1] ? ` (${from[1]})` : ""}` : "", ".", revoked || expired ? " That client was not moved to a new key." : ""),
      h("button", { class: "btn sm", onclick: () => { tab = "refused"; f.who = whoName(e); f.q = e.addr || ""; setParams(); draw(); } }, "Show them")));
  }

  function filtered(list) {
    const q = f.q.toLowerCase();
    return list.filter((e) => (!f.who || (f.who === AGENTS ? e.actor === "agent" : whoName(e) === f.who)) && (!f.area || area(e) === f.area) &&
      (!q || [titleText(e), e.object, e.detail, e.addr, e.client, e.who, e.route, e.reason, e.request_id, whoName(e)].some((x) => x && String(x).toLowerCase().includes(q))))
      .sort((a, b) => b.ts_ms - a.ts_ms);
  }

  function drawList() {
    drawTabsPressed();
    if (auditErr && !audit) { clear(listBox, failure(auditErr, "The audit log")); return; }
    if (!audit) { clear(listBox, h("div", { class: "small dim", style: { padding: "12px 0" } }, "Loading the audit log…")); return; }
    if (tab === "all") { drawAll(); return; }
    const src = tab === "refused" ? audit.refusals : audit.changes;
    const es = filtered(src);
    if (!es.length) {
      const any = src.length;
      clear(listBox, empty(tab === "refused" ? "shield" : "check",
        any ? "Nothing matches these filters" : tab === "refused" ? "No request was refused" : "No changes yet",
        any ? `${fmt.n(any)} ${tab === "refused" ? "refusals" : "changes"} without the filters.` : tab === "refused" ? "Requests without a valid key, or with a key whose scope is too narrow, show here." : "Changes to the cluster, its data, settings and keys show here, with who made them and why.",
        any ? h("button", { class: "btn sm", onclick: () => { f.who = ""; f.area = ""; f.q = ""; search.value = ""; setParams(); draw(); } }, "Clear the filters") : null));
      return;
    }
    const out = [];
    let day = null;
    for (const e of es.slice(0, shown)) {
      const d = dayLabel(e.ts_ms);
      if (d !== day) { day = d; out.push(h("div", { class: "ac-day" }, d)); }
      out.push(entryRow(e));
    }
    if (es.length > shown) out.push(h("div", { class: "ac-more" }, h("button", { class: "btn sm", onclick: () => { shown += 200; drawList(); } }, `Show ${fmt.n(Math.min(200, es.length - shown))} more`), h("span", { class: "small muted" }, ` of ${fmt.n(es.length)}`)));
    clear(listBox, h("div", { class: "ac-list" + (tab === "refused" ? " ref" : "") }, out));
  }
  function drawTabsPressed() { tabsBox.firstChild?.set?.(tab); }

  const idOf = (e) => `${e.ts_ms}|${e.request_id || ""}|${e.route || ""}|${e.who || ""}`;
  function titleText(e) {
    const t = title(e);
    return typeof t === "string" ? t : t.textContent;
  }
  /** The headline: what was done to what, in words. */
  function title(e) {
    if (sortOf(e) === "refusal") return `${e.method || ""} ${e.route || ""}`.trim() || "a request";
    const d = e.detail || "", failed = e.status >= 400;
    if (d && (SQL.test(d) || /^\/v1\/(sql|query)/.test(e.route || ""))) return failed ? "A statement failed" : e.object ? `Ran SQL on ${e.object}` : "Ran SQL";
    // A step of a plan the coordinator applied: what it did; its plan is below.
    if (d && e.after?.plan && e.after?.step) return words((failed ? "tried to " : "") + d.replace(/^plan p\d+, step \d+ of \d+: /, "").replace(/ \(it failed: [^]*\)$/, ""));
    if (d && !failed) return words(d);
    // A cluster change that failed: what it tried (AC-3); its error is below.
    if (d && failed && /^\/v1\/admin\/(shards|rebalance|moves|plan)/.test(e.route || "") && !/^\/v1\/admin\/plan(\/apply|\/run|$)/.test(e.route || "")) return words(`tried to ${d.replace(/^plan (p\d+), /, "")}`);
    if (e.object) return failed ? `A change to ${e.object} failed` : `Changed ${e.object}`;
    const t = ROUTE_TITLES.find(([re]) => re.test(e.route || ""));
    return t ? t[failed ? 2 : 1] : `${e.method || ""} ${e.route || ""}`.trim();
  }
  /** A detail made readable: names for key ids and copies, short versions. */
  function words(d) {
    const ns = names();
    let s = d.replace(/\(([0-9a-f]{16}), (feed|read|write|admin)\)/g, "($2)")
      .replace(/\bkey ([0-9a-f]{16})\b/g, (m, id) => `key ${ns[id] || id.slice(0, 8)}`)
      .replace(/https?:\/\/[^\s,;)]+/g, (u) => (core.copyName ? core.copyName(u) : u))
      .replace(/(?:[a-z0-9.-]+(?::\d+)?\/)+[a-z0-9._-]+(?::[\w.-]+|@sha256:[0-9a-f]{64})/gi, shortImage);
    return s.charAt(0).toUpperCase() + s.slice(1);
  }
  /** Status: done, failed or refused. */
  function status(e) {
    const req = [`${e.method || ""} ${e.route || ""}`.trim(), `answered ${e.status}`, e.ms != null ? `took ${fmt.ms(e.ms)}` : null].filter(Boolean).join(" · ");
    if (sortOf(e) === "refusal") return ["bad", `refused · ${e.status}`, req];
    if (e.status >= 400) return ["bad", `failed · ${e.status}`, req];
    return ["ok", "done", req];
  }
  function bodyOf(e) {
    const out = [];
    if (sortOf(e) === "refusal") {
      const gone = e.status === 401 && revokedKeys().find(keyOf(e));
      const old = e.status === 401 && !gone && liveKeys().find(keyOf(e));
      const said = /^refused: (.+)/.exec(e.detail || "")?.[1];
      out.push(h("div", { class: "dim" }, gone ? `The key was revoked ${at(gone.revoked_ms)}${gone.revoked_by ? ` by ${whoName({ who: gone.revoked_by })}` : ""}.`
        : old?.expires_ms && old.expires_ms <= e.ts_ms ? `The key expired ${at(old.expires_ms)} ${TZ}.`
        : said ? words(said + ".")
        : e.status === 401 ? "No key, or a wrong, expired or revoked one." : `${e.scope ? `A ${e.scope} key` : "This key"} may not do this.`));
      return out;
    }
    const err = e.status >= 400 && typeof e.after?.error === "string" ? e.after.error : null;
    if (e.after?.plan && e.after?.step) {
      // A step of a plan the coordinator applied, and how it ended.
      out.push(h("div", { class: "ac-mono" }, `step ${e.after.step} of ${e.after.of} of plan ${e.after.plan}`));
      if (err) out.push(h("div", { class: "ac-mono ac-bad-t" }, words(err)));
    } else if (err && Object.keys(e.after).length === 1) {
      // Why a change failed, as its answer said.
      out.push(h("div", { class: "ac-mono ac-bad-t" }, words(err)));
    } else if (e.before === undefined && e.after && typeof e.after === "object" && !Array.isArray(e.after)) {
      // Something made: what it was made with, on one line.
      const f = flat(e.after);
      out.push(h("div", { class: "ac-mono" }, Object.entries(f).filter(([k]) => k !== "id").map(([k, v], i) => [i ? " · " : "", h("span", { class: "muted" }, said(k) + " "), leaf(k, v)])));
    } else if (e.after === undefined && e.before && typeof e.before === "object" && !Array.isArray(e.before)) {
      // Something removed: what it was, on one line.
      out.push(h("div", { class: "ac-mono muted" }, Object.entries(flat(e.before)).map(([k, v], i) => [i ? " · " : "", said(k) + " ", leaf(k, v)])));
    } else if (e.before !== undefined || e.after !== undefined) {
      const ds = differs(e.before, e.after);
      const row = ([p, b, a]) => h("div", { class: "ac-mono" }, h("span", { class: "ac-path" }, said(p)), " ",
        b !== undefined ? h("span", { class: "ro-patch-old" }, leaf(p, b)) : null, b !== undefined && a !== undefined ? " → " : "",
        a !== undefined ? h("span", { class: "ro-patch-new" }, leaf(p, a)) : h("span", { class: "muted" }, " removed"));
      out.push(...ds.slice(0, 4).map(row));
      if (ds.length > 4) out.push(h("details", { class: "ac-moredf" }, h("summary", { class: "small muted" }, `${ds.length - 4} more`), ...ds.slice(4).map(row)));
      if (!ds.length) out.push(h("div", { class: "ac-mono muted" }, "no difference"));
    } else if (e.detail && (SQL.test(e.detail) || /^\/v1\/(sql|query)/.test(e.route || ""))) {
      out.push(h("div", { class: "ac-mono ac-sql", title: e.detail }, e.detail));
    } else if (e.detail && e.status >= 400) {
      out.push(h("div", { class: "ac-mono" }, words(e.detail)));
    } else if (!e.detail && !e.object) {
      out.push(h("div", { class: "ac-mono muted" }, `${e.method} ${e.route}`));
    }
    return out;
  }
  function raw(e) {
    const rid = e.request_id;
    return h("dl", { class: "ac-raw" }, [
      ["request", h("span", { class: "mono" }, `${e.method || ""} ${e.route || ""}`)],
      ["answered", `${e.status}${e.ms != null ? ` in ${fmt.ms(e.ms)}` : ""}`],
      ["when", whenT(e.ts_ms)],
      e.key ? ["key id", h("span", { class: "mono" }, e.key)] : null,
      e.addr ? ["address", h("span", { class: "mono" }, e.addr)] : null,
      e.coordinator && manyCoords ? ["kept by", e.coordinator] : null,
      e.client ? ["client", clientName(e.client) === "UI" ? "the command center" : e.client] : null,
      e.detail && title(e) !== words(e.detail) ? ["detail", h("span", { class: "mono" }, e.detail)] : null,
      rid ? ["request id", h("a", { class: "mono", href: core.link ? core.link("logs", { request: rid }) : "#/logs?request=" + encodeURIComponent(rid) }, rid)] : null,
    ].filter(Boolean).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]));
  }
  function entryRow(e) {
    const id = idOf(e), open = opened.has(id);
    const [cls, st, req] = status(e);
    const ar = area(e);
    const row = h("div", { class: "ac-e" + (open ? " open" : "") + (auto(e) ? " auto" : ""), tabIndex: 0, role: "button", "aria-expanded": String(open), title: open ? "" : "Click for the request" },
      h("div", { class: "ac-e-t" }, h("time", { title: whenT(e.ts_ms), datetime: new Date(e.ts_ms).toISOString() }, hm(e.ts_ms))),
      h("div", { class: "ac-e-who" }, h("b", {}, whoName(e)), h("div", { class: "small muted" }, whoSub(e))),
      h("div", { class: "ac-e-what" },
        h("div", { class: "ac-e-title" }, h("span", {}, title(e)), " ", ar !== "other" ? h("span", { class: "badge ac-area " + ar }, AREA[ar]) : null),
        ...bodyOf(e),
        e.reason ? h("div", { class: "ac-e-why" }, `"${e.reason}"`) : null,
        open ? raw(e) : null),
      h("div", { class: "ac-e-st " + cls, title: req }, st));
    const toggle = () => { if (open) opened.delete(id); else opened.add(id); row.replaceWith(entryRow(e)); };
    row.onclick = (ev) => { if (ev.target.closest("a,button,summary,details")) return; if (getSelection()?.toString()) return; toggle(); };
    row.onkeydown = (ev) => { if (ev.key === "Enter" || ev.key === " ") { ev.preventDefault(); toggle(); } };
    return row;
  }

  // The full log: one row a request, columns that say what they hold.
  const allTable = table([
    { key: "ts_ms", label: "time", num: true, render: (e) => h("time", { class: "nowrap", title: whenT(e.ts_ms) }, at(e.ts_ms)) },
    { key: "who", label: "who", sort: whoName, render: (e) => h("span", { class: "ac-allwho" }, h("b", {}, whoName(e)), h("div", { class: "small muted ellipsis", title: whoSub(e) }, whoSub(e))) },
    { key: "route", label: "request", title: "What was asked, what kind of entry it is, the HTTP status the server answered and how long it took", render: (e) => { const k = sortOf(e); return h("span", {}, h("span", { class: "mono small" }, `${e.method} ${e.route}`),
      h("div", { class: "small muted nowrap" }, h("span", { class: "badge" + (k === "refusal" ? " ac-bad" : k === "change" ? " accent" : "") }, k), " ",
        h("span", { class: e.status >= 400 ? "ac-bad-t" : "", title: "HTTP status" }, e.status), e.ms != null ? ` · ${fmt.ms(e.ms)}` : "")); } },
    { key: "detail", label: "what", render: (e) => h("span", { class: "ac-allwhat" },
      e.detail || e.object ? h("span", { class: "small ellipsis ac-what", title: e.detail || e.object }, e.detail ? words(e.detail) : e.object) : h("span", { class: "muted" }, "–"),
      e.request_id ? h("div", {}, h("a", { class: "mono small muted", title: "Opens the request's log lines", href: core.link ? core.link("logs", { request: e.request_id }) : "#/logs?request=" + encodeURIComponent(e.request_id) }, `request ${e.request_id.slice(0, 8)}`)) : null) },
  ], { sortKey: "ts_ms", desc: true, compact: true, max: 500, empty: empty("shield", "Nothing audited yet", "Changes, refusals and noted requests show here.") });
  allTable.classList.add("ac-alltable");
  function drawAll() {
    const src = audit.kinds ? everything : audit.all;
    if (!src) { clear(listBox, h("div", { class: "small dim", style: { padding: "12px 0" } }, "Loading every request…")); return; }
    const es = filtered(src);
    clear(listBox, h("div", { class: "small muted", style: { margin: "0 0 6px" } },
      audit.kinds ? "Changes, refusals and the reads a handler notes (an export, a profile)." : "Every admin request this server recorded, reads among them; the newest 500 show.",
      es.length > 500 ? ` ${fmt.n(es.length)} match: narrow them with the filters.` : ""), allTable);
    allTable.update(es);
  }

  // ---------------------------------------------------------- keys
  function focusKey(id) {
    const tr = keysBox.querySelector(`[data-key="${CSS.escape(id)}"]`);
    if (!tr) return;
    tr.scrollIntoView({ block: "center", behavior: "smooth" });
    tr.classList.add("ac-flash");
    setTimeout(() => tr.classList.remove("ac-flash"), 1600);
  }
  function drawKeys() {
    const sub = keysCard.querySelector(".ac-keys-sub");
    if (keysErr && !keysJ) { sub.textContent = ""; clear(keysBox, h("div", { style: { padding: "0 14px 14px" } }, failure(keysErr, "The API keys"))); return; }
    if (!keysJ) { clear(keysBox, h("div", { class: "small dim", style: { padding: "4px 14px 14px" } }, "Loading the keys…")); return; }
    const live = liveKeys(), gone = revokedKeys(), rots = rotations();
    const listedRevoked = (keysJ.keys || []).some((k) => k.revoked_ms);
    const nRev = listedRevoked ? gone.length : (keysJ.revoked ?? gone.length);
    sub.textContent = `${fmt.n(live.length)} in use` + (nRev ? ` · ${fmt.n(nRev)} revoked` : "") + (listedRevoked ? " · revoked keys stay listed" : gone.length ? " · revoked ones as the audit log remembers them" : "");
    // Successors right under the key they replace.
    const order = live.slice().sort((a, b) => a.name.localeCompare(b.name) || a.created_ms - b.created_ms);
    const rows = [];
    for (const k of order) {
      if (rots.some((x) => x.next.id === k.id)) continue;
      rows.push(keyRow(k, rots));
      for (const x of rots.filter((x) => x.old.id === k.id)) rows.push(keyRow(x.next, rots));
    }
    const a = state.whoami?.auth || {};
    if (a.token) rows.push(tokenRow());
    for (const k of gone.sort((x, y) => y.revoked_ms - x.revoked_ms).slice(0, 12)) rows.push(revokedRow(k));
    const hidden = nRev - Math.min(12, gone.length);
    clear(keysBox,
      rows.length ? h("div", { class: "tbl-wrap" }, h("table", { class: "t compact ac-ktable" },
        h("thead", {}, h("tr", {}, h("th", {}, "name"), h("th", {}, "scope"), h("th", { class: "num" }, "last used"), h("th", { class: "num" }, "expires"), h("th", {}, ""))),
        h("tbody", {}, rows)))
        : empty("access", "No API keys", "Clients use the server's token. Create a key for each, with only the scope it needs.", gateBtn(h("button", { class: "btn sm primary", onclick: () => create() }, icon("plus"), "Create the first key"), "admin")),
      hidden > 0 ? h("div", { class: "small muted", style: { padding: "6px 14px 0" } }, `${fmt.n(hidden)} more revoked ${hidden === 1 ? "key" : "keys"}${listedRevoked ? "" : ": this server does not list revoked keys"}.`) : null,
      keysJ.file ? h("div", { class: "small muted", style: { padding: "6px 14px 10px" } }, `Kept hashed in ${keysJ.file}: a secret is shown once, when it is made.`)
        : h("div", { class: "small muted", style: { padding: "6px 14px 10px" } }, "Kept hashed: a secret is shown once, when it is made."));
  }
  function gateBtn(b, scope) { if (core.gate) return core.gate(b, scope); if (!can(scope)) { b.disabled = true; b.title = needs(scope); } return b; }
  function keyRow(k, rots) {
    const asOld = rots.find((x) => x.old.id === k.id), asNew = rots.find((x) => x.next.id === k.id);
    const flag = unusedAdmin(k);
    const sub = asNew ? (asNew.old.name === k.name ? `the new one, made ${ago(k.created_ms)}` : `successor of ${asNew.old.name}`)
      : k.owner ? (k.owner === k.name ? "their own" : k.owner) : k.owner === undefined ? null : "no owner set";
    const exp = !k.expires_ms ? h("span", { class: k.scope === "admin" && flag ? "ac-warn-t" : "" }, "never")
      : h("span", { class: k.expires_ms < now() ? "ac-bad-t" : expiringSoon(k) ? "ac-warn-t" : "", title: whenT(k.expires_ms) }, k.expires_ms < now() ? "expired" : until(k.expires_ms));
    const used = h("span", { class: flag ? "ac-warn-t" : "", title: k.last_used_ms ? `${whenT(k.last_used_ms)}${k.last_addr ? " · from " + k.last_addr : ""}` : "No request has used it" }, ago(k.last_used_ms));
    const more = h("button", { class: "btn xs ghost", "aria-label": `What to do with ${k.name}`, title: "Rotate, revoke…" }, icon("dots"));
    more.onclick = () => menu(more, [
      !asOld && !asNew ? { label: "Rotate…", icon: "refresh", onclick: () => rotate(k) } : null,
      { label: "Revoke…", icon: "trash", danger: true, onclick: () => revoke(k) },
      "-",
      { label: "Its changes", icon: "filter", onclick: () => { f.who = k.name; tab = "changes"; setParams(); draw(); } },
      { label: "Copy its id", icon: "copy", onclick: () => copy(k.id, "Key id copied") },
    ].filter(Boolean));
    if (!can("admin")) { more.disabled = true; more.title = needs("admin"); }
    return h("tr", { "data-key": k.id, class: asNew ? "ac-succ" : "" },
      h("td", { class: "ac-kname" }, h("b", { title: [`${k.prefix || `sk_${k.id}_`}…`, `made ${whenT(k.created_ms)}`, k.note].filter(Boolean).join(" · ") }, k.name),
        sub || asOld ? h("div", { class: "small muted" }, sub, sub && asOld ? " · " : "", asOld ? h("span", { class: "ac-rotating" }, "rotating") : null) : null),
      h("td", {}, h("span", { class: "ac-scope " + k.scope, title: k.tables ? `limited to ${core.keyLabel(k)}` : null }, k.tables ? (k.preset || "tables") : k.scope),
        k.tables ? h("div", { class: "small muted" }, core.grantsText(k.tables), k.allow?.routes?.length ? ` · routes ${k.allow.routes.join(", ")}` : "", k.allow?.apps?.length ? ` · apps ${k.allow.apps.join(", ")}` : "") : null),
      h("td", { class: "num" }, used, (() => {
        const cs = clientsOf(k.id);
        const title = `Who sent this key: ${callersFrom()}, and the audit log.\n` + cs.map(clientSaid).join("\n");
        if (!cs.length) return k.last_addr ? h("div", { class: "small muted mono" }, k.last_addr) : null;
        return h("div", { class: "small muted ac-clients", title }, cs.length === 1 ? `${cs[0].label}${cs[0].addr ? " · " + cs[0].addr : ""}` : `${cs.length} clients: ${cs.slice(0, 2).map((x) => x.label + (x.addr ? " · " + x.addr : "")).join(", ")}${cs.length > 2 ? ", …" : ""}`);
      })()),
      h("td", { class: "num" }, exp),
      h("td", { class: "ac-kact" }, flag ? gateBtn(h("button", { class: "btn xs danger", onclick: () => revoke(k) }, "Revoke…"), "admin") : null, more));
  }
  function tokenRow() {
    const t = tokenUsed();
    return h("tr", { class: "ac-token" },
      h("td", { class: "ac-kname" }, h("b", {}, "the server token"), h("div", { class: "small muted" }, "--token, shared by every server")),
      h("td", {}, h("span", { class: "ac-scope root" }, "root")),
      h("td", { class: "num" }, h("span", { title: t ? whenT(t) : "No change or refusal in the log names it" }, t ? ago(t) : "–")),
      h("td", { class: "num" }, "never"), h("td", {}));
  }
  function revokedRow(k) {
    const by = k.revoked_by ? ` by ${whoName({ who: k.revoked_by })}` : "";
    const refused = (audit?.refusals || []).filter((e) => keyOf(e)(k)).sort((a, b) => b.ts_ms - a.ts_ms)[0];
    return h("tr", { class: "ac-revoked", "data-key": k.id },
      h("td", { class: "ac-kname" }, h("b", { title: k.fromLog ? "From the audit log: this server does not list revoked keys" : "" }, k.name), h("div", { class: "small muted" }, `revoked ${at(k.revoked_ms)}${by}`)),
      h("td", {}, k.scope ? h("span", { class: "ac-scope " + k.scope }, k.scope) : "–"),
      h("td", { class: "num" + (refused ? " ac-said" : "") }, refused ? h("span", { class: "ac-bad-t", title: whenT(refused.ts_ms) }, `refused ${ago(refused.ts_ms)}`) : h("span", { title: k.last_used_ms ? whenT(k.last_used_ms) : "" }, k.last_used_ms ? ago(k.last_used_ms) : "–")),
      h("td", { class: "num" }, "–"), h("td", {}));
  }

  function drawRotations() {
    const rots = keysJ ? rotations() : [];
    clear(rotBox, rots.map(({ old, next }) => {
      const nextUsed = next.last_used_ms > 0;
      const oldSince = old.last_used_ms > next.created_ms;   // the old key still used after the new one was made
      const moved = nextUsed && !oldSince;
      const step = moved ? 3 : 2;
      const made = (audit?.changes || []).find((e) => (e.detail || "").includes(next.id) || (e.object === `key ${next.name}` && Math.abs(e.ts_ms - next.created_ms) < 5000));
      const ends = old.expires_ms && old.expires_ms > now() && old.expires_ms - now() < 14 * DAY;
      const gone = old.expires_ms && old.expires_ms <= now();
      const refused = gone ? (audit?.refusals || []).filter((e) => keyOf(e)(old) && e.ts_ms >= old.expires_ms).sort((x, y) => y.ts_ms - x.ts_ms) : [];
      const revokeBtn = gateBtn(h("button", { class: "btn sm " + (moved ? "danger" : ""), onclick: () => revoke(old) }, `Revoke ${old.name}…`), "admin");
      const li = (n, done, head, ...rest) => h("li", { class: done ? "done" : n === step ? "now" : "" }, h("span", { class: "ac-n" + (done ? " ok" : "") }, done ? icon("check") : String(n)), h("div", {}, h("b", {}, head), " ", ...rest));
      return card({
        title: `Rotate ${old.name}`, sub: h("span", { class: "badge accent" }, `step ${step} of 3`), cls: "ac-rot",
        body: h("ol", { class: "ac-steps" },
          li(1, true, "Successor created", `${ago(next.created_ms)}${made ? ` by ${whoName(made)}` : ""}: `, h("b", {}, next.name), `, same scope (${next.scope})`, next.expires_ms ? `, expires ${until(next.expires_ms)}` : ", never expires", "."),
          li(2, moved, "Move the clients to it.",
            !nextUsed ? `The new key has not been used yet. The old one was last used ${ago(old.last_used_ms)}${old.last_addr ? ` from ${old.last_addr}` : ""}.`
              : oldSince ? `Both are in use: the old key ${ago(old.last_used_ms)}${old.last_addr ? ` from ${old.last_addr}` : ""}, the new one ${ago(next.last_used_ms)}${next.last_addr ? ` from ${next.last_addr}` : ""}.`
                : `Done: the new key was used ${ago(next.last_used_ms)}, and the old one not since the new one was made.`),
          li(3, false, "Revoke the old key",
            gone ? `· it expired ${at(old.expires_ms)} ${TZ}, so it works no more${refused.length ? `: ${fmt.plural(refused.length, "request")} refused with it since, the last ${ago(refused[0].ts_ms)}${refused[0].client ? ` from ${clientName(refused[0].client)}` : ""}` : ""}. Revoking it takes it off the list.`
              : ends ? `· it stops working ${until(old.expires_ms)} (${at(old.expires_ms)} ${TZ}) anyway, and then the clients still on it are refused.`
              : "· this server keeps it working until it is revoked: revoke it once nothing uses it.")),
        foot: [revokeBtn, h("span", { class: "small muted" }, gone ? (refused.length ? "some clients still send it" : "nothing sent it since it expired") : moved ? "nothing used it since the new key was made" : "it is still the key some clients send")],
      });
    }));
  }

  function scopesCard() {
    return h("details", { class: "card ac-scopes" }, h("summary", { class: "hd" }, h("h3", {}, "What each scope may do")),
      h("div", { class: "bd flush" }, h("div", { class: "tbl-wrap" }, h("table", { class: "t compact" },
        h("thead", {}, h("tr", {}, h("th", {}, ""), ...["feed", "read", "write", "admin"].map((x) => h("th", { style: { textAlign: "center" } }, x)))),
        h("tbody", {}, MATRIX.map(([what, ...on]) => h("tr", {}, h("td", { class: "small" }, what), on.map((x) => h("td", { style: { textAlign: "center", color: x ? "var(--good)" : "var(--line-2)" } }, x ? icon("check") : "·")))))))));
  }

  // ---------------------------------------------------------- actions
  async function after() { await Promise.all([loadKeys(), loadAudit()]); refreshWho(); draw(); }

  async function revoke(k) {
    const recent = k.last_used_ms && now() - k.last_used_ms < DAY;
    const ok = await ask({
      title: `Revoke ${k.name}?`, ok: "Revoke", danger: true, reason: recent ? "required" : "optional", wide: false,
      body: [
        h("p", {}, "Requests with ", h("span", { class: "mono" }, `${k.prefix || `sk_${k.id}_`}…`), " are refused from now on. This cannot be undone: a client needs a new key."),
        facts([
          ["scope", k.tables ? `limited to ${core.keyLabel(k)}` : k.scope],
          ["last used", k.last_used_ms ? `${ago(k.last_used_ms)}${k.last_addr ? ` from ${k.last_addr}` : ""}` : "never"],
          k.owner ? ["owner", k.owner] : null,
          clientsOf(k.id).length ? ["sent by", h("div", { class: "stack", style: { gap: "2px" } }, clientsOf(k.id).slice(0, 6).map((x) => h("div", {}, clientSaid(x))),
            h("div", { class: "small muted" }, `From ${callersFrom()} and the audit log; a client quiet since then is not listed.`))] : null,
        ]),
        recent ? h("div", { class: "ro-warnline" }, icon("warn"), `It was used ${ago(k.last_used_ms)}: whatever sent it stops working.`) : null,
      ],
    });
    if (!ok) return;
    try { await api("/v1/admin/keys/" + encodeURIComponent(k.id), { method: "DELETE", reason: ok.reason || undefined }); toast(`${k.name} revoked`, "ok"); }
    catch (e) { toast(e.message, "bad"); }
    after();
  }

  function rotate(k) {
    let grace = 60;
    const graceSeg = seg([[15, "15 min"], [60, "1 h"], [1440, "24 h"], [10080, "7 days"]], grace, (v) => { grace = v; sync(); });
    const ends = h("span");
    const sync = () => { clear(ends, `${at(now() + grace * 60000)} ${TZ} (${until(now() + grace * 60000)})`); };
    sync();
    const lifetime = k.expires_ms ? Math.max(1, Math.round((k.expires_ms - k.created_ms) / DAY)) : null;
    ask({
      title: `Rotate ${k.name}`, ok: "Make the new key", reason: "optional", wide: false,
      body: [
        h("p", {}, "Makes a new key with the same scope", k.owner ? " and owner" : "", ", shows its secret once, and keeps the old key working for a while so its clients can move."),
        facts([
          ["new key", [h("b", {}, k.name), `, the same scope (${k.scope})`, k.owner ? ` and owner (${k.owner})` : ""]],
          ["old key", k.last_used_ms ? `last used ${ago(k.last_used_ms)}${k.last_addr ? ` from ${k.last_addr}` : ""}` : "never used"],
          ["grace", graceSeg],
          ["old stops", ends],
        ]),
      ],
    }).then(async (ok) => {
      if (!ok) return;
      let j = null, fallback = false;
      try { j = await api(`/v1/admin/keys/${encodeURIComponent(k.id)}/rotate`, { body: { grace_minutes: grace }, reason: ok.reason || undefined }); }
      catch (e) {
        if (!missing(e)) { toast(e.message, "bad"); return; }
        // A server without rotation: a successor named after it; the old
        // key keeps working until it is revoked (step 3).
        rotateMissing = true; fallback = true;
        const taken = new Set((keysJ?.keys || []).map((x) => x.name));
        const ym = new Date().toISOString().slice(0, 7);
        let name = `${k.name}-${ym}`, i = 2;
        while (taken.has(name)) name = `${k.name}-${ym}-${i++}`;
        try { j = await api("/v1/admin/keys", { body: { name, scope: k.scope, tables: k.tables || undefined, routes: k.tables && !k.preset ? k.allow?.routes : undefined, apps: k.tables && !k.preset ? k.allow?.apps : undefined, preset: k.preset || undefined, prefix: k.preset ? presetPrefix(k) : undefined, expires_days: lifetime, owner: k.owner || undefined, note: `successor of ${k.name}` }, reason: ok.reason || `rotating ${k.name}` }); }
        catch (e2) { toast(e2.message, "bad"); return; }
      }
      showSecret(j, {
        title: `The new key for ${k.name}`,
        note: [fallback ? `This server keeps ${k.name} working until you revoke it: once its clients moved, revoke it from the rotation panel beside the keys.`
          : `${k.name} keeps working until ${at(now() + grace * 60000)} ${TZ}, then its requests are refused.`,
        j.key?.expires_ms && j.key.expires_ms - now() < 14 * DAY ? ` The new key expires ${until(j.key.expires_ms)} too: if that is too soon, make a key with a longer life instead.` : ""].join(""),
      });
      after();
    });
  }

  function showSecret(j, { title = "Your new key", note = null } = {}) {
    const curl = `curl -H "authorization: Bearer ${j.secret}" ${location.origin}/v1/health`;
    const m2 = modal({
      title, wide: true, body: [
        h("div", { class: "notice" }, "Copy it now: the server keeps only its hash, so this is the only time it is shown."),
        h("div", { class: "row nw" }, h("div", { class: "secret grow", "data-secret": "key" }, j.secret), h("button", { class: "btn primary", onclick: () => copy(j.secret, "Key copied") }, icon("copy"), "Copy")),
        h("dl", { class: "kv" }, [["name", j.key?.name], ["scope", j.key?.tables ? `limited to ${core.keyLabel(j.key)}` : j.key?.scope], j.key?.owner ? ["owner", j.key.owner] : null, ["expires", j.key?.expires_ms ? `${full(j.key.expires_ms)} (${until(j.key.expires_ms)})` : "never"]].filter(Boolean).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v ?? "–")])),
        h("div", { class: "small muted" }, "Try it:"), h("div", { class: "row nw" }, h("pre", { class: "code grow", "data-secret": "key" }, curl), h("button", { class: "btn icon sm", onclick: () => copy(curl) }, icon("copy"))),
        note ? h("div", { class: "notice" }, note) : null,
        j.closed ? h("div", { class: "notice" }, "This server was open; it now needs a key for every request. This page keeps working: it signed in with the new key.") : null,
      ], foot: [h("button", { class: "btn primary", onclick: () => m2.close() }, "Done")],
    });
    if (j.closed) { setToken(j.secret); refreshWho(); }
  }

  function create() {
    if (!can("admin")) { toast(`Creating a key ${needs("admin")}`, "bad"); return; }
    let scope = "read", daysN = 90, preset = "";
    // Which tables: every one (a scope), chosen ones (POST /v1/admin/keys
    // {tables: {orders: "read"}}), or a job's, a prefix and *
    // ({preset, prefix}).
    const CHOSEN = "*tables";
    const presets = state.whoami?.presets || [];
    const prefix = h("input", { class: "input", placeholder: "team_*", style: { width: "100%" }, maxLength: 128 });
    const presetWhat = h("div", { class: "small muted" });
    const presetBox = h("div", { class: "f", style: { display: "none" } }, h("label", { class: "f" }, "Its tables: a prefix and *", prefix), presetWhat);
    const grants = new Map();
    const grantList = h("div", { class: "f", style: { maxHeight: "240px", overflowY: "auto", gap: "6px" } }, h("div", { class: "small muted" }, "Reading the tables…"));
    const pattern = h("input", { class: "input grow", placeholder: "logs_* (optional)", maxLength: 128, "aria-label": "A pattern of tables, a prefix and *" });
    let patternScope = "read";
    const patternSeg = seg([["read", "read"], ["write", "write"], ["admin", "admin"]], patternScope, (v) => (patternScope = v), { "aria-label": "What it may do to the tables of the pattern" });
    const grantRow = (t) => h("div", { class: "row nw", style: { gap: "8px", justifyContent: "space-between" } },
      h("span", { class: "mono" }, t),
      seg([["", "none"], ["read", "read"], ["write", "write"], ["admin", "admin"]], grants.get(t) || "", (v) => (v ? grants.set(t, v) : grants.delete(t)), { "aria-label": `What it may do to ${t}` }));
    const tablesBox = h("div", { class: "f", style: { display: "none" } },
      h("span", { class: "small muted" }, "What it may do to each table. It cannot see the others, nor the server's settings."),
      grantList,
      h("div", { class: "row nw", style: { gap: "8px" } }, pattern, patternSeg));
    catalogTables().then((ts) => {
      const names = ts.map((t) => t.table).filter((t) => !t.startsWith("_")).sort();
      clear(grantList, names.length ? names.map(grantRow) : h("div", { class: "small muted" }, "No tables yet: name them with a pattern below."));
    }, (e) => clear(grantList, h("div", { class: "small muted" }, `The tables could not be read (${e.message}): name them with a pattern below.`)));
    const presetSeg = seg([["", "every table"], [CHOSEN, "chosen tables"], ...presets.map((p) => [p.name, p.name, p.what])], preset, (v) => {
      preset = v;
      presetBox.style.display = v && v !== CHOSEN ? "" : "none";
      tablesBox.style.display = v === CHOSEN ? "" : "none";
      scopeBox.style.display = v ? "none" : "";
      presetWhat.textContent = v && v !== CHOSEN ? `A key limited to tables that ${presets.find((p) => p.name === v)?.what || ""}.` : "";
    }, { "aria-label": "Which tables it reaches" });
    const name = h("input", { class: "input", placeholder: "search-frontend", style: { width: "100%" }, maxLength: 64 });
    const owner = h("input", { class: "input", placeholder: "search-web team, or a person", style: { width: "100%" } });
    const note = h("input", { class: "input", placeholder: "the frontend's servers in eu-west", style: { width: "100%" } });
    const scopeCards = h("div", { class: "ac-scopecards" });
    let kind = "person", minutes = 60;
    const drawScopes = () => clear(scopeCards, (kind === "agent" ? AGENT_SCOPES : SCOPES).map(([v, t, sub]) => h("button", { type: "button", class: "ac-scopecard" + (scope === v ? " on" : ""), "aria-pressed": String(scope === v), onclick: () => { scope = v; drawScopes(); } }, h("b", {}, t), h("div", { class: "small muted" }, sub))));
    drawScopes();
    const scopeBox = h("div", { class: "f" }, h("span", { class: "small muted" }, "Scope: the least it needs"), scopeCards);
    const exp = seg([[30, "30 days"], [90, "90 days"], [365, "a year"], [null, "never"]], daysN, (v) => (daysN = v));
    const expMin = seg([[60, "an hour"], [240, "4 hours"], [1440, "a day"], [10080, "a week"]], minutes, (v) => (minutes = v));
    const forWhom = h("input", { class: "input", placeholder: "dana: the person it acts for, kept in the audit log", style: { width: "100%" } });
    const agentBox = h("div", { class: "stack", style: { gap: "10px", display: "none" } },
      h("label", { class: "f" }, "Acts for (optional)", forWhom),
      h("div", { class: "f" }, h("span", { class: "small muted" }, "Lasts"), expMin, h("div", { class: "small muted" }, "As long as one task: an agent's key that outlives its task is a key nobody watches.")));
    const personExp = h("div", { class: "f" }, h("span", { class: "small muted" }, "Expires"), exp, h("div", { class: "small muted" }, "A key that expires is rotated on a schedule; one that never expires is flagged when nobody uses it."));
    const presetRow = h("div", { class: "f" }, h("span", { class: "small muted" }, presets.length ? "Which tables: every one, the ones you choose, or a job's (a prefix and *)" : "Which tables: every one, or the ones you choose"), presetSeg);
    const kindSeg = seg([["person", "A person or an app"], ["agent", "An agent"]], kind, (v) => {
      kind = v;
      scope = v === "agent" ? "propose" : "read";
      if (v === "agent" && preset) { preset = ""; presetBox.style.display = "none"; tablesBox.style.display = "none"; scopeBox.style.display = ""; }
      if (presetRow) presetRow.style.display = v === "agent" ? "none" : "";
      agentBox.style.display = v === "agent" ? "" : "none";
      personExp.style.display = v === "agent" ? "none" : "";
      drawScopes();
    }, { "aria-label": "Who uses it" });
    const go = h("button", { class: "btn primary" }, icon("plus"), "Create");
    const m = modal({
      title: "Create an API key", wide: true,
      body: [
        h("div", { class: "row", style: { gap: "12px", alignItems: "flex-start" } },
          h("label", { class: "f grow" }, "Name: what uses it", name),
          h("label", { class: "f grow" }, "Owner: who answers for it", owner)),
        h("label", { class: "f" }, "Note (optional)", note),
        h("div", { class: "f" }, h("span", { class: "small muted" }, "Who uses it"), kindSeg),
        presetRow,
        presetBox,
        tablesBox,
        scopeBox,
        agentBox,
        personExp,
      ],
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go],
    });
    go.onclick = async () => {
      if (!name.value.trim()) { name.focus(); return; }
      go.disabled = true;
      try {
        let limits;
        if (preset === CHOSEN) {
          const tables = Object.fromEntries(grants);
          if (pattern.value.trim()) tables[pattern.value.trim()] = patternScope;
          if (!Object.keys(tables).length) { go.disabled = false; toast("Choose a table it may read, or name a pattern", "bad"); pattern.focus(); return; }
          limits = { tables };
        } else if (preset) {
          if (!prefix.value.trim()) { go.disabled = false; prefix.focus(); return; }
          limits = { preset, prefix: prefix.value.trim() };
        } else limits = { scope };
        const life = kind === "agent" ? { expires_minutes: minutes, agent: scope === "ops" || scope === "propose" ? undefined : true, on_behalf_of: forWhom.value.trim() || undefined } : { expires_days: daysN };
        const j = await api("/v1/admin/keys", { body: { name: name.value.trim(), ...limits, ...life, owner: owner.value.trim() || undefined, note: note.value.trim() || undefined } });
        m.close();
        showSecret(j);
        after();
      } catch (e) { go.disabled = false; toast(e.message, "bad"); }
    };
  }

  function exportCsv() {
    const src = !audit ? [] : tab === "all" ? (audit.kinds ? everything || [] : audit.all) : tab === "refused" ? audit.refusals : audit.changes;
    const es = filtered(src);
    const cols = [["time", (e) => new Date(e.ts_ms).toISOString()], ["who", whoName], ["actor", (e) => e.actor || ""], ["kind", sortOf], ["area", area], ["what", titleText], ["object", (e) => e.object || ""],
      ["before", (e) => (e.before === undefined ? "" : JSON.stringify(e.before))], ["after", (e) => (e.after === undefined ? "" : JSON.stringify(e.after))], ["reason", (e) => e.reason || ""],
      ["client", (e) => e.client || ""], ["address", (e) => e.addr || ""], ["method", (e) => e.method || ""], ["route", (e) => e.route || ""], ["status", (e) => e.status], ["took_ms", (e) => e.ms ?? ""],
      ["request_id", (e) => e.request_id || ""], ["detail", (e) => e.detail || ""]];
    const cell = (v) => { const s = String(v ?? ""); return /[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s; };
    const csv = [cols.map((c) => c[0]).join(","), ...es.map((e) => cols.map(([, g]) => cell(g(e))).join(","))].join("\r\n");
    const a = h("a", { href: URL.createObjectURL(new Blob([csv], { type: "text/csv" })), download: `chloedb-${tab === "all" ? "audit" : tab}-${new Date().toISOString().slice(0, 16).replace(/[:T]/g, "")}.csv` });
    document.body.append(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
    toast(`${fmt.n(es.length)} entries exported`, "ok");
  }

  // ---------------------------------------------------------- start
  draw();
  load();
  if (r.params.get("new")) setTimeout(create, 50);
  // Changes are rare: every 15 s is enough, and a paused page stays still.
  const stop = every(15000, async () => {
    if (document.querySelector(".modal")) return;
    await Promise.all([loadAudit(), loadKeys(), loadCallers()]);
    drawHead(); drawChips(); drawTabs();
    if (!changesCard.contains(document.activeElement) || document.activeElement === search) drawList();
    drawKeys(); drawRotations();
    if (tab === "all" && audit?.kinds) loadAll();
  }, false);
  const offWho = bus.on("whoami", () => { drawHead(); drawChips(); drawKeys(); });
  return () => { stop(); offWho?.(); onChange = null; };
}

/** The prefix a preset key was made for: its grant of admin or write that ends in * and is not *. */
function presetPrefix(k) {
  const own = Object.entries(k.tables || {}).filter(([t, sc]) => t !== "*" && t.endsWith("*") && !t.startsWith("ctx_") && (sc === "admin" || sc === "write"));
  return own[0]?.[0];
}
