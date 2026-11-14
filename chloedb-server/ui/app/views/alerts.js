// Alerts across the fleet. The list shows what fires first, one row a rule
// with where it fires (each copy, a shard, this coordinator), then every
// rule with its scope, owner, runbook and noise, then when each fired over
// the top bar's time range. Each alert has a page, #/alert/<rule>: the
// incident hub, with the metric around it and the events that changed
// things, the blast radius, the failing statements, the log lines grouped
// by message, the affected copies with their links, and acknowledge,
// silence and notes. Rules are edited in a drawer with a backtest over up
// to 7 days; routes carry the messages, each with its delivery status.
// Against a server without the fleet alerting API (no states, routes,
// ack or backtest) the same pages show what that server has and say what
// it cannot do.

import { h, s, icon, clear, fmt, card, api, bus, drawer, toast, pill, seg, empty, menu, copy, showTip, hideTip, table, every, debounce, confirm, can, needs, copyName, findCopy, link, ago, when, whenTitle, ApiError, isNarrow, failure, signInNeeded } from "../core.js";
import { state, refreshAlerts, timeRange, isCoordinator, firesOn } from "../data.js";
import { niceStep } from "../charts.js";
import { pattern } from "./logs.js";

const view = { id: "alerts", title: "Alerts", icon: "alerts", group: "Operate", keywords: "alerts rules firing incidents acknowledge silence runbook owner routes webhook notifications backtest", render, onParams, badge };
export default view;
/** The alert page, `#/alert/<rule>`: not in the navigation. */
export const alertPage = { id: "alert", title: "Alert", icon: "alerts", group: "Operate", hidden: true, nav: false, render, onParams };

// ---------------------------------------------------------------- helpers
const Needs = (scope, route) => needs(scope, route).replace(/^n/, "N");
// Acknowledging, silencing and a note are bounded (tier 1): a write key,
// or an ops key, may take them (the catalog says which routes a role may call).
const ACK = "POST /v1/alerts/:rule/ack";
/** The audit's "change" event of an acknowledge, silence or note: the alert's own event says it already. */
export const dupChange = (e) => e.kind === "change" && /^\/v1\/alerts\/[^/]+\/(ack|silence|notes)$/.test(e.detail?.route || "");
/** Copy URLs in a text, named: "shard http://…:8211: tcp connect error" reads "shard s0-0: tcp connect error". */
const nameUrls = (t) => String(t ?? "").replace(/https?:\/\/[^\s)"',\\]+/g, (u) => { const tail = /[:;.]+$/.exec(u)?.[0] || "", b = tail ? u.slice(0, -tail.length) : u; return (findCopy(b) ? copyName(b) : b) + tail; });
/** A time without seconds: "19:42" today, "Nov 2 19:42" another day. */
const hm = (ms) => when(ms, { sec: false });
const dur = (ms) => fmt.dur(Math.max(0, ms) / 1000).replace(/^(\d+)m 0s$/, "$1 min").replace(/^(\d+)m (\d+)s$/, "$1 min").replace(/^(\d+)s$/, "$1 s");
const alertLink = (id, params) => link("alert/" + encodeURIComponent(id), params);
/** A copy in the cluster by its URL, short name or node name: {shard, i, r, sh}. */
function copyOf(key) {
  const f = findCopy(key);
  if (f) return { shard: f.shard, i: f.copy, r: f.rep, sh: f.sh };
  for (const sh of state.cluster?.shards || []) {
    const i = (sh.replicas || []).findIndex((r) => r.short === key || r.name === key || r.detail?.log_store?.node === key);
    if (i >= 0) return { shard: sh.shard, i, r: sh.replicas[i], sh };
  }
  return null;
}
/** A copy's name for an entry of an alert's `where`: {copy (url), short}. */
const whereName = (c) => findCopy(c.copy) ? copyName(c.copy) : c.short || copyName(c.copy);
/** A server's node name (as the log store names it) as the copy's name. */
function nodeShort(name) {
  const c = copyOf(name);
  return c ? copyName(c.r.url) : name;
}
const coordinator = () => isCoordinator() || !!state.cluster?.shards?.length;
const allCopies = () => (state.cluster?.shards || []).reduce((a, sh) => a + (sh.replicas?.length || 0), 0);
/** One server, no cluster: "each copy" is this server. */
const single = () => !coordinator() && !allCopies();
const range = () => timeRange();
/** Asks before a change, with a required reason: resolves {reason} or false. */
const ask = ({ title, body, ok, danger, typed, reason = true }) => confirm({ title, body, ok, danger, typed, reason });
const failMsg = (e, what) => e instanceof ApiError && e.status === 403 ? `${what} ${needs("write", ACK)}` : e instanceof ApiError && (e.status === 404 || e.status === 405) ? `${what}: this server cannot do that yet (it needs a newer chloedb-server)` : `${what}: ${e.message}`;
const missing = (e) => e instanceof ApiError && (e.status === 404 || e.status === 405);
// A backtest point: the rule's evaluated value over its window (`window`), else the raw value.
const btPoint = (p) => [p.t, p.window !== undefined ? p.window : p.value];

// ---------------------------------------------------------------- metrics
// What a rule can watch: its name, unit and where the value exists (A10).
const METRICS = {
  error_ratio: ["Failed queries, share of all (server-side)", "pct", "both"],
  availability: ["Availability (1 - server failures / queries)", "pct", "both"],
  p99_ms: ["Query p99", "ms", "both"], p95_ms: ["Query p95", "ms", "both"], p50_ms: ["Query p50", "ms", "both"],
  qps: ["Queries a second", "/s", "both"],
  errors_s: ["Failed queries a second (server-side)", "/s", "both"],
  query_errors: ["Failed queries (server-side)", "n", "both"],
  client_errors_s: ["Client errors (400) a second", "/s", "both"],
  timeouts_s: ["Server timeouts a second", "/s", "both"],
  client_deadlines_s: ["Client deadlines passed a second (its own timeout_ms)", "/s", "both"],
  cancelled_s: ["Cancelled queries a second", "/s", "both"],
  rejected_s: ["Queries refused (429) a second", "/s", "both"],
  partial_s: ["Partial answers a second", "/s", "coordinator"],
  shards_no_majority: ["Shards without a majority of copies answering", "n", "coordinator"],
  copies_down: ["Copies not answering", "n", "coordinator"],
  shards_under_replicated: ["Shards with fewer copies answering than they have", "n", "coordinator"],
  primary_changes: ["Primary changes (failovers)", "n", "coordinator"],
  copies_silent: ["Copies whose alerts stopped arriving", "n", "coordinator"],
  slow_s: ["Slow queries a second", "/s", "both"],
  http_5xx_s: ["HTTP 5xx answers a second", "/s", "both"],
  http_4xx_s: ["HTTP 4xx answers a second", "/s", "both"],
  writes_s: ["Writes a second", "/s", "both"],
  write_errors_s: ["Failed writes a second", "/s", "both"],
  write_p99_ms: ["Write p99", "ms", "both"],
  cpu: ["CPU busy (cores)", "n", "both"],
  disk_used_ratio: ["Disk used", "pct", "copy"],
  failing: ["Database refuses writes (0 or 1)", "flag", "copy"],
  flush_failing: ["Flushes fail (0 or 1)", "flag", "copy"],
  merge_failing: ["Merges fail (0 or 1)", "flag", "copy"],
  replica_behind_max: ["Replica behind, in transactions", "n", "copy"],
  replica_behind_s: ["Replica behind, in seconds", "s", "copy"],
  replicas_out_of_sync: ["Replicas out of sync", "n", "copy"],
  serves_reads: ["Serves reads (0 or 1)", "flag", "copy"],
  query_memory_ratio: ["Query memory used", "pct", "both"],
  write_queue: ["Writes waiting", "n", "copy"],
  follow_lag_s: ["Follower behind its source, in seconds", "s", "both"],
  follow_behind: ["Follower behind its source, in transactions", "n", "both"],
  running: ["Queries running", "n", "both"],
  rss_bytes: ["Resident memory", "bytes", "both"],
  heap_bytes: ["Heap", "bytes", "both"],
  segments: ["Segments", "n", "copy"],
  // Made by the server of other values (alerts2).
  http_5xx_ratio: ["HTTP 5xx answers, share of requests", "pct", "both"],
  disk_hours_to_full: ["Hours until the disk is full, at the window's rate", "h", "copy"],
  key_error_ratio: ["The worst key's share of failed calls (keys with 30 calls or more)", "pct", "both"],
  key_busy_ratio: ["The busiest key's queries at once, per core (a key with over half the query time, while others query)", "n", "both"],
  // The rest of what the points hold, named.
  http_s: ["HTTP requests a second", "/s", "both"],
  rows_s: ["Rows written a second", "/s", "both"],
  alloc_bytes_s: ["Memory allocated a second", "bytes/s", "both"],
  allocs_s: ["Allocations a second", "/s", "both"],
  max_ms: ["Slowest query of the 2 s", "ms", "both"],
  live: ["Queries live (running or queued)", "n", "both"],
  slots_free: ["Query slots free (--max-concurrent)", "n", "both"],
  heap_peak_bytes: ["Heap at its peak", "bytes", "both"],
  query_memory_bytes: ["Query memory held", "bytes", "both"],
  query_memory_limit_bytes: ["Query memory limit (--query-memory-mb)", "bytes", "both"],
  queries: ["Queries ended in the 2 s", "n", "both"],
  client_errors: ["Client errors in the 2 s", "n", "both"],
  client_deadlines: ["Client deadlines passed in the 2 s", "n", "both"],
  docs: ["Documents", "n", "copy"],
  deleted_docs: ["Documents deleted, not merged away yet", "n", "copy"],
  memtable_docs: ["Documents in the memtable", "n", "copy"],
  file_bytes: ["Database file size", "bytes", "copy"],
  wal_bytes: ["Write-ahead log size", "bytes", "copy"],
  reusable_bytes: ["Free pages in the file", "bytes", "copy"],
  merges_running: ["Merges running", "n", "copy"],
  merge_memory_bytes: ["Memory merges hold", "bytes", "copy"],
  leases: ["Snapshots leased to coordinators", "n", "copy"],
  pool_queued: ["Query tasks waiting for a thread", "n", "both"],
  disk_total_bytes: ["Disk size", "bytes", "copy"],
  disk_free_bytes: ["Disk free", "bytes", "copy"],
  replicas: ["Replicas of this primary", "n", "copy"],
  replicas_in_sync: ["Replicas in sync", "n", "copy"],
  seq_no: ["Sequence number (writes committed)", "n", "copy"],
  busy: ["Cores busy", "n", "both"],
};
/** Rules built in on every server, by id: what each watches, for the editor's hints. */
const FLAGS = new Set(["failing", "flush_failing", "merge_failing", "serves_reads"]);
const unitOf = (m) => METRICS[m]?.[1] || (/_ms$/.test(m) ? "ms" : /_ratio$/.test(m) ? "pct" : /_s$/.test(m) ? "/s" : /bytes/.test(m) ? "bytes" : "n");
/** A metric's value with its unit. */
function fv(metric, v) {
  if (v == null || Number.isNaN(v)) return "–";
  switch (unitOf(metric)) {
    case "pct": return fmt.pct(v, Math.abs(v) < 0.1 ? 2 : 1);
    case "ms": return fmt.ms(v);
    case "/s": return fmt.si(v) + "/s";
    case "bytes": return fmt.bytes(v);
    case "s": return fmt.dur(v);
    case "h": return v >= 8760 ? "not filling" : v >= 48 ? `${fmt.n(Math.round(v / 24))} days` : `${v < 10 ? v.toFixed(1) : Math.round(v)} h`;
    case "bytes/s": return fmt.bytes(v) + "/s";
    case "flag": return v > 0 ? "on" : "off";
    default: return fmt.si(v);
  }
}
/** A rule's value: against the window before, a multiple ("1.5x"). */
const fvr = (r, v) => r.vs === "before" ? (v == null || Number.isNaN(v) ? "–" : `${+Number(v).toFixed(2)}x`) : fv(r.metric, v);
const EVAL = { all: "each 2-s point", mean: "mean over", max: "max over", min: "min over", rate: "rate over" };
const OPS = [">", ">=", "<", "<="];
const SEV = ["critical", "warning", "info"];
const RANK = { firing: 0, pending: 1, no_data: 2, ok: 3, disabled: 4 };
const SEVR = { critical: 0, warning: 1, info: 2 };
const holds = (v, op, t) => v != null && (op === ">" ? v > t : op === ">=" ? v >= t : op === "<" ? v < t : v <= t);
/** The condition in words: "error_ratio, mean over 5 min > 2%, held 2 min"; "failing is on (any point in 30 s)" (NEW-13). */
function cond(r, short) {
  const ev = r.eval || (r.window_s ? "mean" : "all");
  const w = dur((r.window_s || 300) * 1000);
  const held = r.for_s ? ` for ${dur(r.for_s * 1000)}` : "";
  if (FLAGS.has(r.metric)) {
    // A 0/1 value: "> 0" reads "is on", "< 1" reads "is off".
    const is = r.op.startsWith("<") ? "is off" : "is on";
    return `${short ? "" : r.metric + " "}${is} (${ev === "all" ? "every point" : ev === "max" || ev === "min" ? "a point" : EVAL[ev]} in ${w})${held}`.replace(/\s+/g, " ").trim();
  }
  if (r.vs === "before") return `${short ? "" : r.metric + ", "}${EVAL[ev] || ev} ${w} ${r.op} ${fmt.n(Number(r.threshold))} times the ${w} before${held}`.replace(/\s+/g, " ").trim();
  const how = ev === "all" ? (short ? "" : "each point ") : `${EVAL[ev]} ${w} `;
  return `${short ? "" : r.metric + ", "}${how}${r.op} ${fv(r.metric, r.threshold)}${held}`.replace(/\s+/g, " ").trim();
}
const sevBadge = (sv) => h("span", { class: "al-sev " + (sv || "info") }, sv || "info");

// ---------------------------------------------------------------- the model
// Old servers answer {rules: [{..., status}], firing: [rules], events, webhook};
// new ones {rules, states, events, firing: n, routes}. Both become one shape.
const stateName = (x) => x === "no data" ? "no_data" : x || "ok";
function model(j) {
  // The bell's brief answer (data.js, on other views) is not the whole.
  if (!j || j.brief) return null;
  const fleet = Array.isArray(j.states) || Array.isArray(j.routes);
  const rules = (j.rules || []).map((r) => ({ ...r, status: undefined }));
  const st = new Map();
  for (const x of j.states || []) st.set(x.rule, { ...x, state: stateName(x.state), where: x.where || [], notes: x.notes || [] });
  for (const r of j.rules || []) {
    if (st.has(r.id)) continue;
    const x = r.status || {};
    st.set(r.id, { rule: r.id, state: stateName(x.state), value: x.value, since_ms: x.since_ms || null, where: [], notes: [] });
  }
  for (const r of rules) {
    const x = st.get(r.id) || { rule: r.id, where: [], notes: [] };
    if (!r.enabled) x.state = "disabled";
    x.silent = !!(x.silenced && (!x.silenced.until_ms || x.silenced.until_ms > Date.now()));
    st.set(r.id, x);
  }
  const events = (j.events || []).map(normEvent).filter((e) => e.rule);
  const routes = Array.isArray(j.routes) ? j.routes : j.webhook ? [{ name: "default", legacy: true }] : [];
  return { rules, st, events, routes, fleet, file: j.file, pager: j.pager || null, stateFile: j.state_file || null, rulesBy: j.rules_by || null, rulesMs: j.rules_ms || null };
}
/** With several coordinators, the one that sends the messages (NEW-alerts-logs-2): {sends, me, n} or null. */
function pagerOf(m) {
  const p = m?.pager;
  if (!p?.coordinators || p.coordinators.length < 2 || !p.sends) return null;
  return { sends: p.sends, me: p.this === p.sends, n: p.coordinators.length };
}
const hostOf = (u) => { try { return new URL(u).host; } catch { return String(u || ""); } };
/** Who paged an alert, in words: "paged by 10.0.0.5:7700 at 19:42". */
const pagedText = (x) => x?.paged ? `paged by ${hostOf(x.paged.by)} at ${hm(x.paged.at_ms)}` : null;
/** The copies whose rule differs from the coordinator's (NEW-alerts-logs-1): "2 copies have not taken the last edit". */
function differsText(x) {
  const n = x?.copies?.rule_differs || 0;
  if (!n) return null;
  const failed = x.copies.push_failed || [];
  return `${fmt.plural(n, "copy", "copies")} still on the rule before its last edit${failed.length ? `; sending it failed on ${failed.slice(0, 3).map((f) => f.short || copyName(f.copy)).join(", ")}: ${nameUrls(failed[0].error)}` : "; it is being sent again"}. Their values are judged by the rule as it is now.`;
}
/** An event of /v1/alerts or /v1/events as {ts, rule, what, severity, value, by, text}. */
function normEvent(e) {
  const kind = e.kind || "";
  const what = e.state === "firing" || kind === "alert_fired" || e.what === "fired" ? "fired"
    : e.state === "resolved" || kind === "alert_resolved" || e.what === "resolved" ? "resolved"
    : kind === "alert_acked" || e.state === "acked" ? "acked" : kind === "alert_silenced" || e.state === "silenced" ? "silenced"
    : kind === "note" || kind === "alert_noted" || e.state === "note" || e.state === "noted" ? "note" : e.state || kind || "changed";
  return { ts: e.ts_ms || e.at_ms, rule: e.rule || e.where?.rule, what, severity: e.severity, value: e.value, by: e.who || e.by, text: e.detail?.text || e.text || e.note || e.reason || null, name: e.name, where: e.where, title: e.title };
}
function sorted(m) {
  return m.rules.slice().sort((a, b) => {
    const x = m.st.get(a.id), y = m.st.get(b.id);
    return (x.silent - y.silent) || (RANK[x.state] ?? 5) - (RANK[y.state] ?? 5) || (SEVR[a.severity] ?? 3) - (SEVR[b.severity] ?? 3) || String(a.name).localeCompare(b.name);
  });
}
/** Where a rule fires, in words, and the copies it fires on. */
function whereText(r, x) {
  // It fires on another coordinator's own traffic only (R2-scenarios-1).
  const on = firesOn(x);
  if (on) return `on coordinator ${on}`;
  const all = x.where || [];
  const firing = all.filter((c) => stateName(c.state) === "firing");
  const w = firing.length ? firing : all.filter((c) => stateName(c.state) === "pending");
  const pend = firing.length ? all.filter((c) => stateName(c.state) === "pending").length : 0;
  const names = w.map(whereName);
  const more = pend ? ` (+${pend} pending)` : "";
  // A key's calls: the key, its share of the query time or of failed calls, and its owner.
  if (w.length && w[0].key != null && w[0].running != null) return w.map((c) => `key ${c.key}: ${fmt.pct(c.share)} of the query time, ${c.running.toFixed(1)} queries at once on ${fmt.plural(c.cores, "core")}${c.owner ? `, owner ${c.owner}` : ""}`).join("; ");
  if (w.length && w[0].key != null) return w.map((c) => `key ${c.key}: ${fmt.pct(c.value)} of ${fmt.n(c.calls)} calls failed${c.owner ? `, owner ${c.owner}` : ""}`).slice(0, 2).join("; ") + (w.length > 2 ? ` +${w.length - 2}` : "");
  if (w.length === 1) { const c = copyOf(w[0].copy || w[0].short); return `${names[0]}${c ? ` · shard ${c.shard}` : ""}${more}`; }
  if (r.scope === "each_copy") {
    const total = Math.max(all.length, allCopies());
    if (single()) return "this server";
    if (!w.length) return x.state === "no_data" ? "no copy reports it" : `each copy (${fmt.n(total)})`;
    return `${w.length} of ${fmt.n(total)} copies: ${names.slice(0, 3).join(", ")}${names.length > 3 ? ` +${names.length - 3}` : ""}${more}`;
  }
  if (names.length) return names.slice(0, 3).join(", ") + (names.length > 3 ? ` +${names.length - 3}` : "") + more;
  return r.builtin ? "the cluster" : scopeText(r)[0];
}
function scopeText(r) {
  if (r.scope === "each_copy") return single() ? ["this server", "its own points"] : ["each copy", `${fmt.n(allCopies())} copies`];
  if (r.builtin) return ["from the cluster state", "on this coordinator"];
  const coord = coordinator();
  const n = state.fleet?.coordinators?.length;
  return coord ? [n > 1 ? "coordinators" : "this coordinator", n > 1 ? `${n} coordinators` : "its own points"] : ["this server", "its own points"];
}

let current = null; // the list's model, for the badge
export function badge() {
  const m = model(state.alerts);
  if (!m) return null;
  // Neither silenced nor held back under its cause (the server's firing_shown).
  const firing = m.rules.filter((r) => { const x = m.st.get(r.id); return x.state === "firing" && !x.silent && !x.held_by; });
  if (!firing.length) return null;
  return { n: firing.length, tone: firing.some((r) => r.severity === "critical") ? "bad" : "warn" };
}

// ---------------------------------------------------------------- the list
let openRule = null, openRoutes = null, hubParams = null;
// The list (#/alerts) and an alert's page (#/alert/<rule>, which the router hands to this view
// with the rule in r.rest) share one view: going from one to the other only changes the params,
// so onParams mounts the other one in the same page.
let mount = null;
const hubOf = (r) => { const id = r.rest?.[0] ? decodeURIComponent(r.rest[0]) : null; return id || (r.id === "alert" ? r.params.get("rule") : null) || null; };
function mountInto(r) {
  try { mount.stop?.(); } catch (e) { console.error(e); }
  clear(mount.page);
  mount.id = hubOf(r);
  mount.stop = mount.id ? alertHub(mount.page, mount.id, r) : listView(mount.page, r);
}
function render(page, r) {
  const me = mount = { page, id: null, stop: null };
  mountInto(r);
  return () => { try { me.stop?.(); } catch (e) { console.error(e); } if (mount === me) mount = null; };
}
function onParams(r) {
  if (mount && hubOf(r) !== mount.id) { mountInto(r); mount.page.parentElement?.scrollTo?.(0, 0); return; }
  if (mount?.id) { hubParams?.(r); return; }
  if (r.params.get("rule")) openRule?.(r.params.get("rule"));
  else if (r.params.get("new")) openRule?.(null, newOpts(r));
  if (r.params.get("routes")) openRoutes?.();
}
/** A new rule's start from the link: a query to watch (T5: an alert over any table). */
const newOpts = (r) => r.params.get("sql") ? { sql: r.params.get("sql"), name: r.params.get("name") || "", threshold: Number(r.params.get("threshold") || 0) } : {};

function listView(page, r) {
  const sub = h("p");
  const routesBtn = h("button", { class: "btn", onclick: () => routesDrawer() }, icon("route"), "Routes");
  const newBtn = h("button", { class: "btn primary", onclick: () => editRule(null) }, icon("plus"), "New rule");
  const sum = h("div");
  const firingBox = h("div");
  const silencedBox = h("div");
  const rulesBox = h("div");
  const histBox = h("div");
  const histSub = h("span", { class: "sub" });
  const rulesSub = h("span", { class: "sub" });
  let filter = "all", q = "";
  const search = h("input", { class: "input sm", type: "search", placeholder: "Filter rules, owners, metrics", style: { width: "220px" }, "aria-label": "Filter rules", oninput: debounce(() => { q = search.value.trim().toLowerCase(); drawRules(); }, 150) });
  const filters = seg([["all", "all"], ["firing", "firing"], ["noisy", "noisy"], ["no_data", "no data"], ["builtin", "built-in"], ["norunbook", "no runbook"], ["off", "off"]], filter, (v) => { filter = v; drawRules(); }, { "aria-label": "Show rules" });
  page.append(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Alerts"), sub), h("div", { class: "tools" }, routesBtn, newBtn)),
    sum,
    card({ title: "Firing now", sub: "firing first, then pending · not silenced", body: [firingBox, silencedBox], flush: true, attrs: { style: { marginBottom: "14px" } } }),
    h("section", { class: "card", style: { marginBottom: "14px" } }, h("div", { class: "hd al-rhd" }, h("h3", {}, "Rules"), rulesSub, h("div", { class: "tools" }, filters, search)), h("div", { class: "bd flush" }, rulesBox)),
    h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "When each rule fired"), histSub), h("div", { class: "bd" }, histBox)));

  let m = null, longEvents = null, longFor = null;
  async function loadLong() {
    // Fired and resolved over the top bar's range, from the event store when the server has one.
    const t = range();
    const key = `${Math.round(t.from / 60e3)}-${t.live ? "now" : Math.round(t.to / 60e3)}`;
    if (key === longFor) return;
    longFor = key;
    try {
      const j = await api(`/v1/events?from=${Math.round(t.from)}&to=${Math.round(t.to)}&kind=alert_fired,alert_resolved,alert_acked,alert_silenced&limit=5000`);
      longEvents = (j.events || []).map(normEvent).filter((e) => e.rule);
    } catch { longEvents = null; }
    draw();
  }
  const eventsIn = () => {
    const t = range();
    const all = [...(longEvents || []), ...(m?.events || [])];
    const seen = new Set();
    return all.filter((e) => { const k = `${e.rule}/${e.what}/${e.ts}`; if (seen.has(k)) return false; seen.add(k); return e.ts >= t.from && e.ts <= t.to + 5e3; }).sort((a, b) => a.ts - b.ts);
  };

  function draw() {
    if (state.paused && m) return;
    loadRunbooks(draw);
    m = current = model(state.alerts);
    if (!m) { clear(firingBox, signInNeeded() ? failure(new ApiError(401, "a key is needed"), "Alerts") : empty("alerts", "Loading the rules…")); return; }
    const adm = can("admin");
    newBtn.disabled = !adm; newBtn.title = adm ? "" : Needs("admin");
    clear(routesBtn, icon("route"), m.fleet ? `Routes: ${m.routes.length}` : m.routes.length ? "Route: one webhook" : "Routes: none");
    const coord = coordinator();
    clear(sub, m.fleet
      ? "Rules live on the coordinator, every change audited; rules of scope each copy run on every copy and the coordinator gathers them. What fires is grouped by rule, with where it fires."
      : `This server checks its rules on its own points every 2 seconds${coord ? ": a coordinator cannot see its copies' disks, flushes or replication, so those rules have no data here" : ""}.`);
    drawSummary(); drawFiring(); drawRules(); drawHistory();
  }

  function drawSummary() {
    const firing = m.rules.filter((r) => { const x = m.st.get(r.id); return x.state === "firing" && !x.silent && !x.held_by; });
    const heldAll = m.rules.filter((r) => { const x = m.st.get(r.id); return x.state === "firing" && !x.silent && x.held_by; });
    // Held under a cause that fires, or waiting out its window after the cause resolved.
    const settleN = heldAll.filter((r) => m.st.get(r.id).held_until_ms).length, heldN = heldAll.length - settleN;
    // One severity (F2): an info alert is counted as info, not as a warning.
    const crit = firing.filter((r) => r.severity === "critical"), warn = firing.filter((r) => r.severity === "warning"), info = firing.filter((r) => r.severity !== "critical" && r.severity !== "warning");
    const unacked = crit.filter((r) => !m.st.get(r.id).ack);
    const noData = m.rules.filter((r) => m.st.get(r.id).state === "no_data");
    const bits = [];
    // Copies that stop reporting are themselves an alert (copy-silent); say how many report.
    const silentCopies = (state.cluster?.shards || []).flatMap((sh) => (sh.replicas || []).map((rp, i) => ({ rp, i, sh }))).filter(({ rp }) => rp.status === "down" || rp.role === "unknown");
    if (state.cluster?.shards) bits.push(`Copies reporting: ${fmt.n(allCopies() - silentCopies.length)} of ${fmt.n(allCopies())}${silentCopies.length ? ` (${silentCopies.slice(0, 3).map(({ rp }) => copyName(rp.url)).join(", ")}${silentCopies.length > 3 ? " …" : ""} silent)` : ""}`);
    for (const rt of m.routes.filter((x) => !x.legacy)) bits.push(rt.last_error && (!rt.last_ok_ms || rt.last_error_ms > rt.last_ok_ms) ? `route ${rt.name}: last send failed` : rt.last_ok_ms ? `route ${rt.name} delivered ${hm(rt.last_ok_ms)}` : `route ${rt.name}: none sent since this server started`);
    if (!m.routes.length) bits.push("no route: nobody is told when an alert fires");
    const pg = pagerOf(m);
    if (pg) bits.push(`${pg.n} coordinators; messages are sent by ${pg.me ? "this one" : hostOf(pg.sends)}`);
    // LIV-3: the copies' own rules (scope self) fire on the copies only; say what fires there.
    const onCopies = state.alerts?.copies_firing || [];
    if (onCopies.length) bits.push(`on the copies' own rules: ${onCopies.slice(0, 4).map((f) => `${f.name} (${f.short || copyName(f.copy)})`).join(", ")}${onCopies.length > 4 ? ` and ${onCopies.length - 4} more` : ""}`);
    if (!firing.length && !heldAll.length) {
      clear(sum, h("div", { class: "banner ok" }, icon("okc", "lg"), h("div", { class: "grow" },
        h("div", { class: "big" }, "Nothing is firing"),
        h("div", { class: "small dim" }, `${fmt.plural(m.rules.length, "rule")} · ${fmt.n(m.rules.filter((r) => m.st.get(r.id).state === "ok").length)} ok${noData.length ? ` · ${fmt.n(noData.length)} without data (they cannot tell)` : ""}${bits.length ? " · " + bits.join(" · ") : ""}`))));
      return;
    }
    const oldest = unacked.reduce((a, r) => Math.min(a, m.st.get(r.id).since_ms || Date.now()), Date.now());
    clear(sum, h("div", { class: "banner " + (crit.length ? "bad" : warn.length ? "warn" : "info") }, icon(crit.length || warn.length ? "warn" : "info", "lg"), h("div", { class: "grow" },
      h("div", { class: "big" }, firing.length ? [crit.length ? `${crit.length} critical` : null, warn.length ? `${warn.length} warning` : null, info.length ? `${info.length} info` : null].filter(Boolean).join(", ") + " firing" + (heldN ? `, ${heldN} more held back under their cause` : "") + (settleN ? `, ${settleN} more waiting out their window after their cause resolved` : "")
        : `${fmt.plural(heldAll.length, "alert")} firing, held back: ${settleN ? "waiting out their window after their cause resolved" : "under their cause"}`,
        m.fleet && unacked.length ? h("span", { style: { fontWeight: 500 } }, ` · ${fmt.plural(unacked.length, "critical alert")} not acknowledged for ${dur(Date.now() - oldest)}`) : null),
      h("div", { class: "small dim" }, bits.join(" · ")))));
  }

  function actions(r, x) {
    const out = [];
    if (m.fleet) {
      const w = can("write", ACK);
      out.push(h("button", { class: "btn xs", disabled: !w || !!x.ack, title: !w ? Needs("write", ACK) : x.ack ? `Acknowledged by ${x.ack.by}` : "Acknowledge: you are on it", onclick: () => ack(r) }, "Ack"));
      const sil = h("button", { class: "btn xs", disabled: !w, title: w ? "Silence it for a while, with a reason" : Needs("write", ACK) }, "Silence", icon("down"));
      sil.onclick = () => silenceMenu(sil, r, draw);
      out.push(sil);
    }
    out.push(h("a", { class: "btn xs", href: alertLink(r.id) }, "Open"));
    return h("div", { class: "row nw al-acts", style: { gap: "4px", justifyContent: "flex-end" } }, out);
  }
  async function ack(r) {
    try { await api(`/v1/alerts/${encodeURIComponent(r.id)}/ack`, { body: {} }); toast(`Acknowledged: ${r.name}`, "ok"); await refreshAlerts(); }
    catch (e) { toast(failMsg(e, "Acknowledging"), "bad"); }
  }

  async function ackAll(list) {
    const ok = await ask({ title: `Acknowledge ${fmt.plural(list.length, "firing alert")}?`, body: `${list.slice(0, 6).map((r) => r.name).join(", ")}${list.length > 6 ? ` and ${list.length - 6} more` : ""}. Each gets your name and the time; their messages are not sent again.`, ok: "Acknowledge all", reason: false });
    if (!ok) return;
    const res = await Promise.allSettled(list.map((r) => api(`/v1/alerts/${encodeURIComponent(r.id)}/ack`, { body: {} })));
    const bad = res.filter((x) => x.status === "rejected");
    if (bad.length) toast(`${fmt.n(list.length - bad.length)} of ${fmt.n(list.length)} acknowledged. ${failMsg(bad[0].reason, "Acknowledging")}`, "bad", 8000);
    else toast(`Acknowledged ${fmt.plural(list.length, "alert")}`, "ok");
    await refreshAlerts();
  }

  function drawFiring() {
    const live = sorted(m).filter((r) => { const x = m.st.get(r.id); return (x.state === "firing" || x.state === "pending") && !x.silent; });
    // A storm (NEW-alerts-logs-4): what a cause explains is held back under it, the cause first.
    const byId = new Map(live.map((r) => [r.id, r]));
    const under = new Map();
    for (const r of live) { const c = m.st.get(r.id).held_by; if (c && byId.has(c)) (under.get(c) || under.set(c, []).get(c)).push(r); }
    const rows = [];
    for (const r of live) {
      const c = m.st.get(r.id).held_by;
      if (c && byId.has(c)) continue;
      rows.push(r);
      for (const d of under.get(r.id) || []) rows.push(d);
    }
    const isHeld = (r) => { const c = m.st.get(r.id).held_by; return !!(c && byId.has(c)); };
    const silenced = m.rules.filter((r) => m.st.get(r.id).silent);
    clear(silencedBox, silenced.length ? h("div", { class: "al-silenced" }, icon("pause"),
      h("span", {}, `${silenced.length} silenced: `, silenced.map((r, i) => { const x = m.st.get(r.id); return [i ? "; " : "", h("a", { href: alertLink(r.id) }, r.name), ` (${x.state === "firing" ? "firing" : x.state}) by ${x.silenced.by || "someone"} until ${hm(x.silenced.until_ms)}${x.silenced.reason ? `, "${x.silenced.reason}"` : ""}`]; }))) : null);
    if (!rows.length) {
      clear(firingBox, h("div", { class: "al-quiet" }, icon("okc"), h("span", {}, "No rule is firing or pending."), m.rules.some((r) => m.st.get(r.id).state === "no_data") ? h("span", { class: "muted" }, "Rules without data are listed below: they cannot fire until their metric has a value.") : null));
      return;
    }
    const unacked = live.filter((r) => { const x = m.st.get(r.id); return x.state === "firing" && !x.ack; });
    const held = rows.filter(isHeld);
    const w8 = can("write", ACK);
    const bar = m.fleet && (unacked.length > 1 || held.length) ? h("div", { class: "al-fbar" },
      h("span", { class: "small" }, [`${fmt.plural(live.length, "alert")}`, held.length ? `${fmt.n(held.length)} held back under their cause` : null, unacked.length ? `${fmt.n(unacked.length)} not acknowledged` : "all acknowledged"].filter(Boolean).join(" · ")),
      unacked.length > 1 ? h("button", { class: "btn sm", disabled: !w8, title: w8 ? "Acknowledge every firing alert that nobody acknowledged" : Needs("write", ACK), onclick: () => ackAll(unacked) }, icon("check"), `Acknowledge all (${fmt.n(unacked.length)})`) : null) : null;
    const tbl = table([
      { key: "sev", label: "", width: "24px", cls: "al-c-dot", render: (r) => h("span", { class: "dot " + (m.st.get(r.id).state === "firing" ? (r.severity === "critical" ? "critical pulse" : r.severity === "warning" ? "warn" : "") : "pending"), title: m.st.get(r.id).state } ) , sortable: false },
      { key: "name", label: "alert", cls: "al-c-name", render: (r) => { const x = m.st.get(r.id); return h("div", { class: isHeld(r) ? "al-heldname" : "" },
        isHeld(r) ? h("span", { class: "al-under", "aria-hidden": "true" }, "└") : null,
        h("div", {}, h("a", { class: "al-name", href: alertLink(r.id) }, r.name),
          h("div", { class: "tiny muted" }, r.builtin ? "built-in · " : "", cond(r, true)),
          isHeld(r) ? h("div", { class: "tiny al-heldwhy" }, `Held back: ${x.held_why || "its cause fires"} explains it. No message of its own while it fires.`)
            : x.held_by && x.held_until_ms ? h("div", { class: "tiny al-heldwhy" }, `Its cause, ${m.rules.find((y) => y.id === x.held_by)?.name || x.held_by}, resolved. This pages at ${hm(x.held_until_ms)} if it still fires then: the ${dur(((r.window_s || 0) + (r.for_s || 0)) * 1000)} it judges still include the outage.`) : null,
          under.get(r.id)?.length ? h("div", { class: "tiny muted" }, `Likely cause of the ${fmt.plural(under.get(r.id).length, "alert")} under it`) : null,
          differsText(x) ? h("div", { class: "tiny al-warn" }, differsText(x)) : null)); } },
      { key: "where", label: "where", cls: "al-c-where", render: (r) => { const x = m.st.get(r.id); return h("span", { class: "small" }, whereText(r, x), pagerOf(m) && x.paged ? h("div", { class: "tiny muted" }, pagedText(x)) : null); } },
      { key: "since", label: "since", cls: "al-c-since", sort: (r) => m.st.get(r.id).since_ms, render: (r) => { const x = m.st.get(r.id); return h("span", { class: "small nowrap", title: whenTitle(x.since_ms) }, x.since_ms ? `${hm(x.since_ms)} · ${dur(Date.now() - x.since_ms)}` : "–"); } },
      { key: "now", label: "now", cls: "al-c-now", render: (r) => { const x = m.st.get(r.id); return h("span", { class: "small nowrap" }, h("span", { class: "al-narrow-only muted" }, "now "), x.value == null ? "no value" : fvr(r, x.value)); } },
      { key: "owner", label: "owner", cls: "al-c-owner", render: (r) => r.owner ? h("span", { class: "small" }, r.owner) : h("span", { class: "small muted" }, "no owner") },
      { key: "state", label: "state", cls: "al-c-state", sort: (r) => m.st.get(r.id).state, render: (r) => ackPill(r, m.st.get(r.id), m.fleet) },
      { key: "act", label: "", cls: "al-c-act", sortable: false, render: (r) => actions(r, m.st.get(r.id)) },
    ], { rows, compact: true, onRow: (r) => { location.hash = alertLink(r.id); }, rowClass: (r) => isHeld(r) ? "al-held" : "" });
    tbl.classList.add("al-ftbl");
    clear(firingBox, bar, tbl);
  }

  function drawRules() {
    if (!m) return;
    const evs = eventsIn();
    const t = range();
    const fires = {}, flaps = {}, open = {};
    for (const e of evs) {
      if (e.what === "fired") { (fires[e.rule] ||= []).push(e.ts); open[e.rule] = e.ts; }
      // A flap (A7): it resolved within 2 minutes of firing.
      else if (e.what === "resolved" && open[e.rule] != null) { if (e.ts - open[e.rule] < 120e3) flaps[e.rule] = (flaps[e.rule] || 0) + 1; open[e.rule] = null; }
    }
    // Noisy: fired 3 times or more over the range, or flapped twice.
    const noisy = (r) => (fires[r.id] || []).length >= 3 || (flaps[r.id] || 0) >= 2;
    let rows = sorted(m);
    if (filter === "firing") rows = rows.filter((r) => ["firing", "pending"].includes(m.st.get(r.id).state));
    else if (filter === "noisy") rows = rows.filter(noisy).sort((a, b) => (fires[b.id] || []).length - (fires[a.id] || []).length);
    else if (filter === "no_data") rows = rows.filter((r) => m.st.get(r.id).state === "no_data");
    else if (filter === "builtin") rows = rows.filter((r) => r.builtin);
    else if (filter === "norunbook") rows = rows.filter((r) => !runbookOf(r));
    else if (filter === "off") rows = rows.filter((r) => !r.enabled);
    if (q) rows = rows.filter((r) => [r.name, r.id, r.metric, r.owner, r.description].some((v) => String(v || "").toLowerCase().includes(q)));
    const adm = can("admin");
    rulesSub.textContent = `${rows.length} of ${m.rules.length} · firing first · fires over ${t.label}`;
    clear(rulesBox, table([
      { key: "severity", label: "sev.", cls: "al-c-sev", sort: (r) => SEVR[r.severity], render: (r) => sevBadge(r.severity) },
      { key: "name", label: "rule", cls: "al-c-name", render: (r) => { const x = m.st.get(r.id), dt = differsText(x); return h("div", { class: "al-rname" }, h("div", { class: "row nw", style: { gap: "6px" } }, h("button", { class: "al-link al-rlink", title: "Edit the rule", onclick: (e) => { e.stopPropagation(); editRule(r); } }, r.name), r.builtin ? h("span", { class: "badge accent", title: "Built in: watches the cluster state; only its severity, on/off and route change" }, "built-in") : null, dt ? h("span", { class: "badge al-wbadge", title: dt }, `${fmt.n(x.copies.rule_differs)} on the old rule`) : null), h("div", { class: "tiny muted mono" }, cond(r))); } },
      { key: "scope", label: "scope", cls: "al-c-scope", sort: (r) => r.scope || "", render: (r) => { const [a, b] = scopeText(r); return h("div", { class: "small" }, a, h("div", { class: "tiny muted" }, b)); } },
      { key: "owner", label: "owner", cls: "al-c-owner", render: (r) => r.owner ? h("span", { class: "small" }, r.owner) : h("span", { class: "small muted" }, "–") },
      { key: "runbook", label: "runbook", cls: "al-c-runbook", render: (r) => { const rb = runbookOf(r); return rb ? h("a", { class: "small", href: rb.url, target: "_blank", rel: "noopener", title: rb.own ? rb.url : `Built in: ${rb.title}` }, rb.own ? rb.title : rb.title.slice(0, 24), " ", icon("ext")) : h("span", { class: "small muted" }, "none"); } },
      { key: "fires", label: "fires · flaps", num: true, cls: "al-c-fires", title: `times it fired over ${t.label}, and of those how often it resolved within 2 minutes (a flap)`, sort: (r) => (fires[r.id] || []).length, render: (r) => h("div", { class: "row nw", style: { gap: "8px", justifyContent: "flex-end" } }, fireBars(fires[r.id] || [], t), h("b", { class: "num" }, fmt.n((fires[r.id] || []).length)), h("span", { class: "num small " + (flaps[r.id] ? "al-warn" : "muted"), title: `${fmt.plural(flaps[r.id] || 0, "flap")}: resolved within 2 minutes of firing` }, `· ${fmt.n(flaps[r.id] || 0)}`), noisy(r) ? h("span", { class: "badge al-wbadge", title: "Fired 3 times or more, or flapped twice, over the range: raise its threshold, hold it longer or give it a resolve threshold" }, "noisy") : null) },
      { key: "state", label: "state", cls: "al-c-state", sort: (r) => RANK[m.st.get(r.id).state], render: (r) => statePill(r, m.st.get(r.id)) },
      { key: "on", label: "on", cls: "al-c-on", sortable: false, render: (r) => onSwitch(r, !adm, (on) => setEnabled(r, on)) },
      { key: "edit", label: "", cls: "al-c-edit", sortable: false, render: (r) => h("button", { class: "btn xs ghost", title: "Edit the rule", onclick: () => editRule(r) }, icon("edit")) },
    ], { rows, compact: true, onRow: (r) => editRule(r), empty: empty("alerts", "No rule matches", "Try another filter.") }));
    rulesBox.firstElementChild?.classList.add("al-rtbl");
  }

  async function setEnabled(r, on) {
    const x = m.st.get(r.id);
    let reason = "";
    if (!on) {
      const ok = await ask({ title: `Turn "${r.name}" off?`, body: x.state === "firing" ? "It is firing now: turning it off resolves it and sends a RESOLVED message. Nothing checks it until it is on again." : "Nothing checks it until it is turned on again. To quiet it for a while, silence it instead.", ok: "Turn off", danger: x.state === "firing" });
      if (!ok) { drawRules(); return; }
      reason = ok.reason || "";
    }
    try { const j = await saveRules(m.rules.map((y) => y.id === r.id ? { ...y, enabled: on } : y), reason); const p = pushedText(j, `${r.name}: ${on ? "on" : "off"}`); toast(...(p || [`${r.name}: ${on ? "on" : "off"}`, "ok"])); }
    catch { drawRules(); }
  }

  function drawHistory() {
    const t = range();
    const evs = eventsIn();
    histSub.textContent = `${t.label}${longEvents ? "" : " · from the latest events this server keeps in memory"}`;
    const spans = {};
    for (const e of evs) {
      if (e.what !== "fired" && e.what !== "resolved") continue;
      const r = m.rules.find((x) => x.id === e.rule);
      const sp = spans[e.rule] ||= { name: r?.name || e.name || e.rule, sev: r?.severity || e.severity, list: [] };
      if (e.what === "fired") sp.list.push([e.ts, null]);
      else if (sp.list.length && sp.list[sp.list.length - 1][1] == null) sp.list[sp.list.length - 1][1] = e.ts;
      else sp.list.push([t.from, e.ts]);
    }
    for (const r of m.rules) { const x = m.st.get(r.id); if (x.state === "firing" && x.since_ms && !spans[r.id]) spans[r.id] = { name: r.name, sev: r.severity, list: [[x.since_ms, null]] }; }
    const rows = Object.entries(spans);
    if (!rows.length) { clear(histBox, h("div", { class: "al-quiet" }, icon("okc"), h("span", {}, `No rule fired over ${t.label}.`))); return; }
    const cw = histBox.clientWidth || 1000, small = cw < 600;
    const W = Math.max(small ? 280 : 500, cw), rh = small ? 26 : 22, LW = small ? 112 : 210, NL = small ? 16 : 30;
    const t0 = t.from, t1 = t.to;
    const X = (v) => LW + (Math.max(t0, Math.min(t1, v)) - t0) / (t1 - t0) * (W - LW - 8);
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${rows.length * rh + 20}`, style: { width: "100%", height: rows.length * rh + 20 + "px" }, role: "img", "aria-label": "When each rule fired" });
    rows.forEach(([id, sp], i) => {
      const a = s("a", { href: alertLink(id) }, s("text", { x: 0, y: i * rh + 15, class: "al-tl-name" }, sp.name.length > NL ? sp.name.slice(0, NL - 1) + "…" : sp.name));
      svgEl.append(a, s("rect", { x: LW, y: i * rh + 5, width: W - LW - 8, height: rh - 10, rx: 3, class: "al-tl-bg" }));
      for (const [a0, a1] of sp.list) {
        const x0 = X(a0), x1 = X(a1 ?? t1);
        const bar = s("rect", { x: x0, y: i * rh + 5, width: Math.max(3, x1 - x0), height: rh - 10, rx: 3, class: "al-tl-bar " + (sp.sev || "info") });
        bar.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, sp.name), h("div", { class: "r" }, h("b", {}, a1 ? dur(a1 - a0) : "still firing"), h("span", { class: "n" }, `${when(a0)} – ${a1 ? when(a1) : "now"}`))]));
        bar.addEventListener("pointerleave", hideTip);
        svgEl.append(bar);
      }
    });
    const nt = small ? 2 : 5;
    for (let k = 0; k <= nt; k++) { const v = t0 + k / nt * (t1 - t0); svgEl.append(s("text", { class: "al-ax", x: X(v), y: rows.length * rh + 15, "text-anchor": k === 0 ? "start" : k === nt ? "end" : "middle" }, t1 - t0 > 86400e3 ? hm(v) : fmt.time(v, false))); }
    clear(histBox, svgEl);
  }

  openRule = (id, o) => { if (id == null) { editRule(null, o || {}); return; } const x = (model(state.alerts)?.rules || []).find((y) => y.id === id); if (x) editRule(x); };
  openRoutes = () => routesDrawer();
  draw();
  refreshAlerts().then(draw);
  loadLong();
  if (r.params.get("rule")) setTimeout(() => openRule(r.params.get("rule")), 60);
  else if (r.params.get("new")) setTimeout(() => editRule(null, newOpts(r)), 60);
  if (r.params.get("routes")) setTimeout(() => routesDrawer(), 60);
  const offs = [bus.on("alerts", draw), bus.on("whoami", () => { m = null; draw(); }), bus.on("cluster", () => { if (!state.paused && m) drawSummary(); }), bus.on("range", () => { longFor = null; loadLong(); draw(); }), bus.on("narrow", () => { if (m) drawHistory(); })];
  const stop = every(30000, () => { if (!state.paused && range().live) { longFor = null; return loadLong(); } }, false);
  return () => { offs.forEach((f) => f()); stop(); openRule = openRoutes = null; };
}

// The runbooks compiled into the server (GET /v1/runbooks, #482): a rule
// without one of its own (the built-in rules) is covered by the one for
// its id, linked to its page of the docs.
let runbookIndex = null, runbooksAsked = null;
const RUNBOOK_DOCS = "https://chloetech-ai.github.io/chloedb/site/docs/runbooks.html#";
/** Reads the index once; `then` runs when it is there (a view redraws). */
function loadRunbooks(then) {
  if (runbookIndex) return;
  runbooksAsked ||= api("/v1/runbooks").then((j) => { runbookIndex = j.index || []; }).catch(() => { runbookIndex = []; });
  runbooksAsked.then(() => then?.());
}
/** The compiled runbook for a rule's id: {title, url}, or null. */
export function builtInRunbook(id, index = runbookIndex) {
  const r = (index || []).find((x) => (x.for || []).includes(id));
  return r ? { title: r.title, url: r.docs || RUNBOOK_DOCS + r.title.toLowerCase().replace(/[^a-z0-9 -]/g, "").trim().replace(/\s+/g, "-") } : null;
}
const runbookOf = (r) => (r.runbook ? { title: runbookName(r.runbook), url: r.runbook, own: true } : builtInRunbook(r.id));
const runbookName = (u) => { try { const p = new URL(u); const last = p.pathname.split("/").filter(Boolean).pop(); return last ? decodeURIComponent(last).slice(0, 24) : p.host; } catch { return u.slice(0, 24); } };
function fireBars(ts, t) {
  const n = 14, bins = new Array(n).fill(0);
  for (const v of ts) bins[Math.min(n - 1, Math.max(0, Math.floor((v - t.from) / (t.to - t.from) * n)))]++;
  const max = Math.max(1, ...bins);
  return s("svg", { viewBox: `0 0 ${n * 5} 16`, width: n * 5, height: 16, class: "al-fires", "aria-hidden": "true" }, bins.map((b, i) => b ? s("rect", { x: i * 5, y: 16 - Math.max(3, b / max * 16), width: 3.5, height: Math.max(3, b / max * 16) }) : null));
}
/** The state in words; no data is its own neutral pill, never "info" (A1). */
function statePill(r, x) {
  if (!r.enabled) return h("span", { class: "pill disabled" }, icon("minus"), "off");
  if (x.silent) return h("span", { class: "pill al-silent", title: `${x.silenced.reason || ""}` }, icon("pause"), `silenced to ${hm(x.silenced.until_ms)}`);
  if (x.state === "firing") return pill(r.severity === "info" ? "warn" : "firing", `firing ${x.since_ms ? dur(Date.now() - x.since_ms) : ""}`.trim());
  if (x.state === "pending") return pill("pending", "pending");
  // Some copies ok, others without the metric (a primary-only or replica-only metric): say both.
  if (x.state === "no_data" && x.copies?.ok > 0) return h("span", { class: "pill al-nodata", title: `${x.copies.ok} of ${x.copies.total} copies are ok. ${x.copies.no_data} have no ${r.metric} value: only primaries or only replicas report some metrics.` }, icon("minus"), `ok on ${x.copies.ok} · no data on ${x.copies.no_data}`);
  if (x.state === "no_data") return h("span", { class: "pill al-nodata", title: "The metric has no value here, so the rule cannot tell. It keeps its last state and never resolves on its own." }, icon("minus"), `no data${x.no_data_since_ms || x.since_ms ? " " + dur(Date.now() - (x.no_data_since_ms || x.since_ms)) : ""}`);
  return pill("ok", "ok");
}
function ackPill(r, x, fleet) {
  if (x.state !== "firing") return pill("pending", "pending");
  if (x.ack) return h("span", { class: "pill ok", title: x.ack.note || "" }, icon("check"), `acked · ${x.ack.by || "someone"}`);
  if (!fleet) return h("span", { class: "pill " + (r.severity === "critical" ? "critical" : "warn") }, `firing ${x.since_ms ? dur(Date.now() - x.since_ms) : ""}`);
  return h("span", { class: "pill " + (r.severity === "critical" ? "critical" : "warn") }, `unacked ${x.since_ms ? dur(Date.now() - x.since_ms) : ""}`);
}
/** A switch with its state in words (A8: the old one rendered as a bare knob). */
function onSwitch(r, disabled, onChange) {
  const inp = h("input", { type: "checkbox", checked: r.enabled, disabled, "aria-label": `${r.name}: checked or not` });
  const lbl = h("span", { class: "al-sw-l" }, r.enabled ? "on" : "off");
  inp.onchange = () => { lbl.textContent = inp.checked ? "on" : "off"; onChange(inp.checked); };
  return h("label", { class: "al-switch" + (disabled ? " off" : ""), title: disabled ? Needs("admin") : r.enabled ? "On: the rule is checked. Turn it off" : "Off: nothing checks it. Turn it on", onclick: (e) => e.stopPropagation() }, inp, h("span", { class: "al-track" }), lbl);
}

function silenceMenu(anchor, r, after) {
  const until9 = () => { const d = new Date(); d.setDate(d.getDate() + (d.getHours() >= 9 ? 1 : 0)); d.setHours(9, 0, 0, 0); return Math.round((d - Date.now()) / 60e3); };
  menu(anchor, [[30, "for 30 minutes"], [60, "for 1 hour"], [240, "for 4 hours"], [until9(), "until 09:00"]].map(([min, label]) => ({ label: `Silence ${label}`, icon: "pause", onclick: () => silence(r, min, after) })));
  const el = [...document.querySelectorAll(".menu")].pop();
  if (el && !isNarrow()) requestAnimationFrame(() => { const b = el.getBoundingClientRect(); if (b.bottom > innerHeight - 8) el.style.top = Math.max(8, anchor.getBoundingClientRect().top - b.height - 4) + "px"; });
}
async function silence(r, minutes, after) {
  const until = Date.now() + minutes * 60e3;
  const ok = await ask({ title: `Silence "${r.name}" until ${hm(until)}?`, body: `It stays on this page, greyed, and keeps its state; no message is sent for it until ${when(until)}. The silence ends by itself.`, ok: "Silence" });
  if (!ok) return;
  try { await api(`/v1/alerts/${encodeURIComponent(r.id)}/silence`, { body: { minutes, reason: ok.reason || "" }, reason: ok.reason }); toast(`Silenced until ${hm(until)}`, "ok"); await refreshAlerts(); after?.(); }
  catch (e) { toast(failMsg(e, "Silencing"), "bad"); }
}

async function saveRules(next, reason) {
  try {
    const j = await api("/v1/alerts/rules", { method: "PUT", body: { rules: next.map(cleanRule) }, reason: reason || undefined });
    await refreshAlerts();
    return j;
  } catch (e) { toast(e instanceof ApiError && e.status === 403 ? `Changing rules ${needs("admin")}` : e.message, "bad"); throw e; }
}
/**
 * What the copies did with a saved edit (NEW-alerts-logs-1): the PUT's
 * `pushed` is {copies, took, failed: [{copy, short, error}]}. Returns
 * [text, tone] for the toast, or null when no each-copy rule changed.
 */
function pushedText(j, what = "Saved") {
  const p = j?.pushed;
  if (!p || !p.copies) return null;
  if (!p.failed?.length) return [`${what}. Every copy took it (${fmt.n(p.took)} of ${fmt.n(p.copies)}).`, "ok", 5000];
  const names = p.failed.map((f) => f.short || copyName(f.copy));
  const why = [...new Set(p.failed.map((f) => nameUrls(f.error)))];
  return [`${what}. ${fmt.n(p.took)} of ${fmt.n(p.copies)} copies took it; ${names.slice(0, 4).join(", ")}${names.length > 4 ? ` +${names.length - 4}` : ""} did not: ${why.slice(0, 2).join("; ")}. The coordinator sends it again every 30 s and judges their values by the new rule meanwhile.`, "warn", 12000];
}
function cleanRule(x) {
  const out = { id: x.id, name: x.name, metric: x.metric, op: x.op, threshold: Number(x.threshold), for_s: Number(x.for_s) || 0, severity: x.severity, enabled: !!x.enabled, description: x.description || "" };
  for (const k of ["eval", "window_s", "resolve", "scope", "owner", "runbook", "route", "builtin", "vs", "sql"]) if (x[k] != null && x[k] !== "") out[k] = x[k];
  if (out.window_s != null) out.window_s = Number(out.window_s);
  if (out.resolve != null) out.resolve = Number(out.resolve);
  return out;
}

// ---------------------------------------------------------------- charts
/**
 * A time chart for the alert page and the backtest: series of [t, v], a
 * threshold and a resolve line, shaded spans when the rule fired, and
 * labelled markers (events) along the top. o: {t0, t1, series: [{name, cls,
 * pts}], thr, resolve, spans: [[a, b]], markers: [{t, label, tone}], fmtv}.
 */
function alertChart(height = 240) {
  const root = h("div", { class: "chart al-chart" });
  const svgEl = s("svg", { height, role: "img", "aria-label": "the metric over time" });
  root.append(svgEl);
  let o = null, width = 0;
  function draw() {
    if (!o) return;
    width = root.clientWidth || 700;
    svgEl.setAttribute("viewBox", `0 0 ${width} ${height}`);
    svgEl.replaceChildren();
    const marks = (o.markers || []).filter((k) => k.t >= o.t0 && k.t <= o.t1).sort((a, b) => a.t - b.t);
    const span = Math.max(1, o.t1 - o.t0);
    const M = { l: 52, r: 14, b: 22, t: 8 };
    const W = width - M.l - M.r;
    const X = (t) => M.l + (t - o.t0) / span * W;
    // Marker labels in lanes along the top, so they do not overlap. The
    // alert's own events get a label first, then copies and shards, then
    // other changes; a marker with no room left keeps only its line.
    // As much of a label as the chart has room for (a phone's chart takes about 40 characters).
    const CW = 6.1, lanes = [], maxLanes = width < 560 ? 3 : 4, maxChars = Math.max(16, Math.min(56, Math.floor((width - 66 - 12) / CW)));
    for (const k of [...marks].sort((a, b) => (a.prio ?? 1) - (b.prio ?? 1) || a.t - b.t)) {
      // The label as shown, and a box as wide as it: what happened, without its numbers
      // ("A shard has no majority fires on shard 1", not ": shards_no_majority = 1 ...") when it is long.
      const colon = k.label.indexOf(": ");
      const base = k.label.length > maxChars && colon > 8 ? k.label.slice(0, colon) : k.label;
      k.shown = base.length > maxChars ? base.slice(0, maxChars - 1) + "…" : base;
      const w = 12 + k.shown.length * CW, x = Math.min(width - M.r - w, Math.max(M.l, X(k.t) - 4));
      const fits = (l) => l.every(([a, b]) => x + w < a - 4 || x > b + 4);
      let lane = lanes.findIndex(fits);
      if (lane < 0 && lanes.length < maxLanes) { lanes.push([]); lane = lanes.length - 1; }
      if (lane < 0) { k.lane = -1; continue; }
      lanes[lane].push([x, x + w]); k.lane = lane; k.x = x; k.w = w;
    }
    M.t = 8 + lanes.length * 17;
    const H = height - M.t - M.b;
    let ymax = 0;
    for (const sr of o.series) for (const [, v] of sr.pts) if (v != null && v > ymax) ymax = v;
    if (o.thr != null) ymax = Math.max(ymax, o.thr * 1.2);
    if (o.resolve != null) ymax = Math.max(ymax, o.resolve * 1.1);
    if (!(ymax > 0)) ymax = 1;
    const step = niceStep(ymax, 4);
    ymax = Math.ceil(ymax / step) * step;
    const Y = (v) => M.t + H - v / ymax * H;
    const g = s("g", { class: "grid" }), ax = s("g", { class: "axis" });
    for (let v = 0; v <= ymax + 1e-12; v += step) { g.append(s("line", { x1: M.l, x2: width - M.r, y1: Y(v), y2: Y(v) })); ax.append(s("text", { x: M.l - 6, y: Y(v) + 3.5, "text-anchor": "end" }, o.fmtv(v))); }
    for (const t of ticks(o.t0, o.t1, W)) ax.append(s("text", { x: X(t), y: height - 6, "text-anchor": "middle" }, span > 86400e3 * 1.5 ? new Date(t).toLocaleDateString(undefined, { month: "short", day: "numeric" }) + (new Date(t).getHours() ? " " + fmt.time(t, false) : "") : fmt.time(t, false)));
    svgEl.append(g);
    for (const [a, b] of o.spans || []) { const x0 = X(Math.max(o.t0, a)), x1 = X(Math.min(o.t1, b ?? o.t1)); if (x1 > M.l && x0 < width - M.r) svgEl.append(s("rect", { class: "al-fired", x: x0, y: M.t, width: Math.max(2, x1 - x0), height: H })); }
    svgEl.append(ax, s("line", { class: "base", x1: M.l, x2: width - M.r, y1: Y(0), y2: Y(0) }));
    for (const sr of o.series) {
      let d = "", open = false;
      for (const [t, v] of sr.pts) { if (v == null || !Number.isFinite(v)) { open = false; continue; } d += (open ? "L" : "M") + X(t).toFixed(1) + "," + Y(Math.min(v, ymax)).toFixed(1); open = true; }
      svgEl.append(s("path", { class: "al-ln " + (sr.cls || ""), d }));
    }
    if (o.thr != null) svgEl.append(s("line", { class: "thr", x1: M.l, x2: width - M.r, y1: Y(o.thr), y2: Y(o.thr) }), s("text", { class: "thr-lbl", x: width - M.r - 4, y: Y(o.thr) - 5, "text-anchor": "end" }, `threshold ${o.fmtv(o.thr)}`));
    if (o.resolve != null && o.resolve !== o.thr) svgEl.append(s("line", { class: "al-resolve", x1: M.l, x2: width - M.r, y1: Y(o.resolve), y2: Y(o.resolve) }), s("text", { class: "al-resolve-l", x: M.l + 4, y: Y(o.resolve) - 4 }, `resolves at ${o.fmtv(o.resolve)}`));
    for (const k of marks) {
      const x = X(k.t);
      svgEl.append(s("line", { class: "al-mk " + (k.tone || ""), x1: x, x2: x, y1: k.lane >= 0 ? 8 + k.lane * 17 + 14 : M.t, y2: M.t + H }));
      if (k.lane >= 0) svgEl.append(s("g", { class: "al-mkl " + (k.tone || "") }, s("rect", { x: k.x, y: 8 + k.lane * 17, width: k.w, height: 14, rx: 3 }), s("text", { x: k.x + 5, y: 8 + k.lane * 17 + 10.5 }, k.shown), s("title", {}, `${when(k.t)} ${k.label}`)));
    }
    // Hover: every series at the nearest point, and the markers near it.
    const xh = s("line", { class: "xhair", y1: M.t, y2: M.t + H, style: { display: "none" } });
    const over = s("rect", { x: M.l, y: M.t, width: W, height: H, fill: "transparent" });
    svgEl.append(xh, over);
    over.addEventListener("pointermove", (e) => {
      const r = svgEl.getBoundingClientRect();
      const t = o.t0 + ((e.clientX - r.left) * (width / r.width) - M.l) / W * span;
      xh.setAttribute("x1", X(t)); xh.setAttribute("x2", X(t)); xh.style.display = "";
      const rows = o.series.map((sr) => { const p = nearest(sr.pts, t); return h("div", { class: "r" }, h("span", { class: "k al-k " + (sr.cls || "") }), h("b", {}, p ? o.fmtv(p[1]) : "–"), h("span", { class: "n" }, sr.name)); });
      const near = marks.filter((k) => Math.abs(k.t - t) < span / W * 8).map((k) => h("div", { class: "r" }, h("span", { class: "n", style: { marginLeft: 0, paddingLeft: 0 } }, `${hm(k.t)} ${k.label}`)));
      showTip(e.clientX, e.clientY, [h("div", { class: "t" }, when(t)), ...rows, ...near]);
    });
    over.addEventListener("pointerleave", () => { xh.style.display = "none"; hideTip(); });
  }
  new ResizeObserver(() => { if (root.isConnected && Math.abs(root.clientWidth - width) > 2) draw(); }).observe(root);
  root.update = (x) => { o = x; draw(); };
  return root;
}
function nearest(pts, t) {
  let lo = 0, hi = pts.length - 1;
  if (hi < 0) return null;
  while (lo < hi) { const mid = (lo + hi) >> 1; if (pts[mid][0] < t) lo = mid + 1; else hi = mid; }
  if (lo > 0 && Math.abs(pts[lo - 1][0] - t) < Math.abs(pts[lo][0] - t)) lo--;
  return pts[lo][1] == null ? null : pts[lo];
}
const STEPS = [60e3, 120e3, 300e3, 600e3, 900e3, 1800e3, 3600e3, 7200e3, 10800e3, 21600e3, 43200e3, 86400e3];
function ticks(t0, t1, w) {
  const want = Math.max(2, Math.floor(w / 95));
  const st = STEPS.find((x) => (t1 - t0) / x <= want) || 86400e3;
  const off = st >= 86400e3 ? new Date(t0).getTimezoneOffset() * 60e3 : 0;
  const out = [];
  for (let t = Math.ceil((t0 - off) / st) * st + off; t <= t1; t += st) out.push(t);
  return out;
}

// ---------------------------------------------------------------- evaluation
/** The value a rule tests at each point, from raw [t, v] points (as the server does). */
function evaluate(pts, r) {
  const ev = r.eval || (r.window_s ? "mean" : "all");
  if (ev === "all") return pts;
  const w = (r.window_s || 300) * 1000, out = [];
  let j = 0;
  for (let i = 0; i < pts.length; i++) {
    while (pts[j][0] <= pts[i][0] - w) j++;
    const win = pts.slice(j, i + 1).filter((p) => p[1] != null);
    if (!win.length) { out.push([pts[i][0], null]); continue; }
    const vs = win.map((p) => p[1]);
    out.push([pts[i][0], ev === "max" ? Math.max(...vs) : ev === "min" ? Math.min(...vs) : vs.reduce((a, b) => a + b, 0) / vs.length]);
  }
  return out;
}
/** When a rule fires on evaluated points: [[from, to|null]], with for_s and the resolve threshold. */
function firings(pts, r) {
  const spans = [];
  let pend = null, on = false;
  const res = r.resolve != null && r.resolve !== "" ? Number(r.resolve) : Number(r.threshold);
  for (const [t, v] of pts) {
    if (v == null) continue; // no data keeps the state
    if (on) { if (!holds(v, r.op, res) && !(r.op.startsWith(">") ? v > res : v < res)) { spans[spans.length - 1][1] = t; on = false; pend = null; } continue; }
    if (holds(v, r.op, Number(r.threshold))) { pend ??= t; if (t - pend >= (Number(r.for_s) || 0) * 1000) { spans.push([pend, null]); on = true; } }
    else pend = null;
  }
  return spans;
}
const median = (xs) => { if (!xs.length) return null; const a = xs.slice().sort((x, y) => x - y); return a[Math.floor(a.length / 2)]; };

/** Raw points of a metric over [from, to): the server's series (per minute beyond its hour), else the browser's. */
async function seriesOf(metric, from, to) {
  let pts = null, source = "memory";
  try {
    const j = await api(`/v1/timeseries?from=${Math.round(from)}&to=${Math.round(to)}`);
    pts = (j.points || []).filter((p) => p.t >= from && p.t <= to);
    if (j.source) source = j.source;
  } catch { /* the browser's own */ }
  if (!pts?.length) pts = (state.points || []).filter((p) => p.t >= from && p.t <= to);
  return { pts: pts.map((p) => [p.t, metricOf(p, metric)]), source, has: pts.some((p) => metricOf(p, metric) != null) };
}
function metricOf(p, metric) {
  const v = p[metric];
  if (v != null) return v;
  if (metric === "error_ratio" && p.queries) return p.query_errors != null ? p.query_errors / p.queries : null;
  if (metric === "availability" && p.queries) return 1 - (p.query_errors || 0) / p.queries;
  // Made of others, as the server makes them (alerts.rs value_of).
  if (metric === "http_5xx_ratio" && p.http_s > 0) return (p.http_5xx_s || 0) / p.http_s;
  if (metric === "replica_behind_s" && p.replica_behind_max != null) return p.replica_behind_max <= 0 ? 0 : p.writes_s > 0 ? Math.min(86400, p.replica_behind_max / p.writes_s) : 86400;
  return null;
}

// ---------------------------------------------------------------- the alert page
const TONE = { alert_fired: "bad", alert_resolved: "good", alert_acked: "good", alert_silenced: "muted", copy_down: "bad", majority_lost: "bad", copy_up: "good", majority_back: "good", primary_changed: "warn", rollout: "accent", upgrade: "accent", change: "accent", log_level: "accent", autoscale: "accent", contain: "warn", note: "muted", moves_started: "accent", moves_finished: "accent", shard_added: "accent", shard_removed: "accent", copy_added: "accent", copy_removed: "accent" };
const WINDOWS = [["15m", "±15 min", 15 * 60e3], ["1h", "±1 h", 3600e3], ["6h", "±6 h", 6 * 3600e3], ["7d", "7 days", 7 * 86400e3]];

function alertHub(page, id, r) {
  id = decodeURIComponent(id || "");
  if (!id) { location.hash = "#/alerts"; return; }
  const fixedAtOpen = range().fixed;
  let win = r?.params?.get("w") || (fixedAtOpen ? "range" : null), m = null, one = null, rule = null, x = null, gen = 0, notesAt = null;
  const crumbs = h("div", { class: "row small muted al-crumbs" }, h("a", { href: link("alerts") }, "Alerts"), h("span", {}, "›"), h("span", { class: "mono" }, id));
  const title = h("h1", { class: "row nw al-htitle", style: { gap: "10px" } });
  const line = h("p");
  const tools = h("div", { class: "tools al-hubbar" });
  const kpis = h("div", { class: "grid g4 al-kpis" });
  const chart = alertChart(250);
  const chartNote = h("div", { class: "small muted", style: { marginTop: "6px" } });
  const winSeg = seg([...(fixedAtOpen ? [["range", "the link's window"]] : []), ...WINDOWS.map(([k, l]) => [k, l])], win, (v) => { win = v; loadChart(); }, { "aria-label": "Time around the alert" });
  const chartSub = h("span", { class: "sub" });
  const blast = h("div");
  const changed = h("div"), changedSub = h("span", {}, "events from 30 min before it fired");
  const stmts = h("div");
  const copies = h("div");
  const logsBox = h("div");
  const logsLink = h("a", { class: "btn sm ghost" }, "Open in Logs", icon("right"));
  const activity = h("div");
  const noteIn = h("input", { class: "input", placeholder: "Add a note: what you found, what you did", style: { flex: "1 1 auto" } });
  const noteBtn = h("button", { class: "btn" }, "Post");
  page.append(
    crumbs,
    h("div", { class: "page-head", style: { marginBottom: "12px" } }, h("div", {}, title, line), tools),
    kpis,
    h("div", { class: "grid g3 al-row" },
      h("section", { class: "card span2" }, h("div", { class: "hd" }, h("h3", {}, "Around the alert"), chartSub, h("div", { class: "tools" }, winSeg)), h("div", { class: "bd" }, chart, chartNote)),
      card({ title: "Blast radius", sub: "since it fired", body: blast })),
    h("div", { class: "grid g3 al-row" },
      card({ title: "What changed", sub: changedSub, body: changed }),
      card({ title: "Failing statements", sub: "server failures, since it fired", tools: h("a", { class: "btn sm ghost", href: link("queries", { tab: "statements" }) }, "Queries", icon("right")), body: stmts, flush: true }),
      card({ title: "Affected copies", tools: h("a", { class: "btn sm ghost", href: link("cluster") }, "Cluster", icon("right")), body: copies, flush: true })),
    h("div", { class: "grid g3 al-row" },
      card({ title: "Log lines, grouped", sub: "warn and error, every server, since 5 min before it fired", tools: logsLink, body: logsBox, flush: true, cls: "span2" }),
      card({ title: "Activity", sub: "this alert's timeline, shared with the link", body: [activity, h("div", { class: "row nw", style: { marginTop: "10px" } }, noteIn, noteBtn)] })),
    h("div", { class: "al-hubspace", "aria-hidden": "true" }));

  const since = () => x?.since_ms && (x.state === "firing" || x.state === "pending") ? x.since_ms : lastFired() || Date.now() - 3600e3;
  const lastFired = () => { const f = (one?.events || m?.events || []).filter((e) => e.rule === id && e.what === "fired").map((e) => e.ts); return f.length ? Math.max(...f) : null; };

  // The page as built, for when sign-in or an error replaced it (NEW-alerts-logs-5).
  const built = [...page.childNodes];
  let blocked = false;
  async function load() {
    const my = ++gen;
    let failed = null;
    m = model(state.alerts);
    if (!m) m = model(await api("/v1/alerts").catch((e) => { failed = e; return null; }));
    try {
      const j = await api(`/v1/alerts/${encodeURIComponent(id)}`);
      one = { ...j, events: (j.events || []).map(normEvent), notes: j.notes || j.state?.notes || [] };
    } catch (e) { one = null; failed ||= missing(e) ? null : e; }
    if (my !== gen) return;
    rule = m?.rules.find((y) => y.id === id) || (one?.rule && typeof one.rule === "object" ? one.rule : null);
    if (!rule) {
      // Not signed in (a link from a message opens here first): say so, and load again after sign-in.
      blocked = true;
      const e401 = failed instanceof ApiError && failed.status === 401;
      clear(page, crumbs, e401
        ? empty("lock", "Sign in to see this alert", `This server needs a token or an API key. After you sign in, the alert "${id}" opens here.`, h("button", { class: "btn sm primary", onclick: () => bus.emit("unauthorized") }, "Sign in"))
        : failed && !m ? empty("alerts", "The alerts did not load", failMsg(failed, "Loading the alerts"), h("button", { class: "btn sm", onclick: () => load() }, "Try again"))
          : empty("alerts", `No rule "${id}"`, "It may have been deleted or renamed.", h("a", { class: "btn sm", href: link("alerts") }, "All alerts")));
      return;
    }
    if (blocked) { blocked = false; clear(page, ...built); loadChart(); loadStatements(); loadLogs(); }
    const fromOne = one && (one.state && typeof one.state === "object" ? one.state : one);
    x = { ...(m.st.get(id) || {}), ...(fromOne?.state && typeof fromOne.state === "string" ? { ...fromOne, state: stateName(fromOne.state) } : {}) };
    x.silent = !!(x.silenced && (!x.silenced.until_ms || x.silenced.until_ms > Date.now()));
    if (!rule.enabled) x.state = "disabled";
    if (win == null) { win = x.since_ms && Date.now() - x.since_ms < 20 * 60e3 && (x.state === "firing" || x.state === "pending") ? "15m" : "1h"; winSeg.set(win); }
    drawHead(); drawKpis(); drawCopies(); drawActivity();
  }

  function drawHead() {
    document.title = `${rule.name} · Alert · ChloeDB`;
    const st = x.state;
    const tone = st === "firing" ? (rule.severity === "critical" ? "critical pulse" : "warn") : st === "pending" ? "pending" : st === "ok" ? "ok" : "";
    clear(title, h("span", { class: "dot " + tone, style: { width: "10px", height: "10px" } }), rule.name,
      st === "firing" ? h("span", { class: "pill " + (rule.severity === "critical" ? "critical" : "warn") }, `${rule.severity} · firing ${x.since_ms ? dur(Date.now() - x.since_ms) : ""}`) : statePill(rule, x),
      rule.builtin ? h("span", { class: "badge accent" }, "built-in") : null);
    const w = (x.where || []).filter((c) => c.state === "firing");
    const cause = x.held_by ? m.rules.find((y) => y.id === x.held_by) : null;
    const pg = pagerOf(m), dt = differsText(x);
    clear(line, cond(rule), " · ", rule.scope === "each_copy" && !single() ? h("b", { class: "al-strong" }, w.length ? `on ${w.length} of ${fmt.n(Math.max((x.where || []).length, allCopies()))} copies` : `on each copy (${fmt.n(allCopies())})`) : h("b", { class: "al-strong" }, (rule.metric === "key_error_ratio" || rule.metric === "key_busy_ratio") && w.length ? whereText(rule, x) : firesOn(x) ? `on coordinator ${firesOn(x)}` : scopeText(rule)[0]),
      x.since_ms && st !== "ok" ? ` · since ${when(x.since_ms)}` : "", ` · now ${x.value == null ? "no value" : fvr(rule, x.value)}`, h("span", { class: "al-peak" }),
      x.held_by && x.held_until_ms && (st === "firing" || st === "pending") ? h("div", { class: "small al-heldwhy" }, "Its cause, ", h("a", { href: alertLink(x.held_by) }, cause?.name || x.held_by), `, resolved. This pages at ${hm(x.held_until_ms)} if it still fires then: the ${dur(((rule.window_s || 0) + (rule.for_s || 0)) * 1000)} it judges still include the outage.`)
      : x.held_by && (st === "firing" || st === "pending") ? h("div", { class: "small al-heldwhy" }, "Held back under its cause, ", h("a", { href: alertLink(x.held_by) }, cause?.name || x.held_by), `${/\(on [^)]*\)$/.exec(x.held_why || "")?.[0] ? " " + /\(on [^)]*\)$/.exec(x.held_why)[0] : ""}. It sends no message of its own while its cause fires, and pages if it still fires after.`) : null,
      dt ? h("div", { class: "small al-warn" }, dt) : null,
      rule.metric === "key_busy_ratio" && w[0]?.key ? h("div", { class: "small" }, h("a", { href: link("queries", { tab: "live", group: "key", q: w[0].key }) }, `Key ${w[0].key}'s queries in Running now`, icon("right"))) : null,
      firesOn(x) ? h("div", { class: "small al-heldwhy" }, `It ${st === "pending" ? "waits to fire" : "fires"} on coordinator ${firesOn(x)}, on the traffic sent through it; on this coordinator it is ${stateName(x.here) === "ok" ? "fine" : stateName(x.here).replace("_", " ")}. `,
        ...(x.elsewhere || []).filter((e) => e.url).slice(0, 2).map((e) => h("a", { href: e.url.replace(/\/$/, "") + "/#/alert/" + encodeURIComponent(rule.id), target: "_blank", rel: "noopener" }, `Open it on ${e.name || hostOf(e.url)}`, icon("ext")))) : null,
      pg && st === "firing" ? h("div", { class: "small muted" }, x.paged ? `${pagedText(x)[0].toUpperCase()}${pagedText(x).slice(1)}.` : `Not paged yet. Messages are sent by ${pg.me ? "this coordinator" : hostOf(pg.sends)}.`) : null);
    const w8 = can("write", ACK), fleet = m.fleet;
    const ackBtn = x.ack
      ? h("button", { class: "btn al-acked", disabled: true, title: x.ack.note || "" }, icon("check"), h("span", { class: "al-wide-only" }, `Acknowledged · ${x.ack.by || "someone"} ${hm(x.ack.at_ms)}`), h("span", { class: "al-narrow-only" }, "Acked"))
      : h("button", { class: "btn primary", disabled: !fleet || !w8 || st !== "firing", title: !fleet ? "This server cannot acknowledge alerts yet" : !w8 ? Needs("write", ACK) : st !== "firing" ? "Only a firing alert is acknowledged" : "Acknowledge: you are on it", onclick: async () => {
        try { await api(`/v1/alerts/${encodeURIComponent(id)}/ack`, { body: {} }); toast("Acknowledged", "ok"); await refreshAlerts(); load(); } catch (e) { toast(failMsg(e, "Acknowledging"), "bad"); }
      } }, icon("check"), h("span", { class: "al-wide-only" }, "Acknowledge"), h("span", { class: "al-narrow-only" }, "Ack"));
    const silBtn = x.silent
      ? h("button", { class: "btn", disabled: !w8, title: `Silenced by ${x.silenced.by || "someone"}: ${x.silenced.reason || ""}`, onclick: async () => {
        const ok = await ask({ title: "End the silence now?", body: "Messages for this alert are sent again from now on.", ok: "End the silence" });
        if (!ok) return;
        try { await api(`/v1/alerts/${encodeURIComponent(id)}/silence`, { method: "DELETE", reason: ok.reason }); toast("Silence ended", "ok"); await refreshAlerts(); load(); } catch (e) { toast(failMsg(e, "Ending the silence"), "bad"); }
      } }, icon("pause"), h("span", { class: "al-wide-only" }, `Silenced to ${hm(x.silenced.until_ms)} · end`), h("span", { class: "al-narrow-only" }, `To ${hm(x.silenced.until_ms)} · end`))
      : h("button", { class: "btn", disabled: !fleet || !w8, title: !fleet ? "This server cannot silence alerts yet" : !w8 ? Needs("write", ACK) : "Silence it for a while, with a reason" }, icon("pause"), "Silence", icon("down"));
    if (!x.silent) silBtn.onclick = () => silenceMenu(silBtn, rule, () => load());
    clear(tools, ackBtn, silBtn,
      h("button", { class: "btn", disabled: !fleet || !w8, title: !fleet ? "This server keeps no notes yet" : !w8 ? Needs("write", ACK) : "Add a note to the alert's timeline", onclick: () => { noteIn.scrollIntoView({ block: "center", behavior: "smooth" }); noteIn.focus({ preventScroll: true }); } }, icon("edit"), h("span", { class: "al-wide-only" }, "Add note"), h("span", { class: "al-narrow-only" }, "Note")),
      h("button", { class: "btn al-ico", title: "Copy a link to this alert at this time", onclick: () => { const t = since(); copy(location.origin + location.pathname + alertLink(id, { from: Math.round(t - 30 * 60e3), to: Math.round(x.state === "firing" ? Date.now() : t + 60 * 60e3) }), "Link copied: it opens this alert at this time"); } }, icon("link"), h("span", { class: "al-wide-only" }, "Copy link")),
      h("button", { class: "btn ghost al-ico", title: "Edit the rule", onclick: () => editRule(rule, { after: load }) }, icon("sliders"), h("span", { class: "al-wide-only" }, "Edit rule")),
      can("admin", "POST /v1/admin/oncall") ? h("a", { class: "btn al-ico", href: "#/oncall?alert=" + encodeURIComponent(id) + (st === "firing" || st === "pending" ? "&start=1" : ""), title: st === "firing" || st === "pending" ? "The on-call agent looks into it now, with read-only tools, and proposes what to do" : "Ask the on-call agent about this alert" }, icon("agent"), h("span", { class: "al-wide-only" }, "Investigate")) : null);
    noteIn.disabled = noteBtn.disabled = !fleet || !w8;
    noteIn.placeholder = !fleet ? "Notes need a newer server" : !w8 ? Needs("write", ACK) : "Add a note: what you found, what you did";
  }

  function drawKpis() {
    loadRunbooks(drawKpis);
    // Who is told: the rule's own route, or else every route that takes its severity (as the server sends).
    const takes = (rt) => rt.legacy || !rt.severities?.length || rt.severities.includes(rule.severity);
    const told = rule.route ? m.routes.filter((rt) => rt.name === rule.route) : m.routes.filter(takes);
    const failing = (rt) => rt.last_error && (!rt.last_ok_ms || (rt.last_error_ms || 0) > rt.last_ok_ms);
    const route = told.length === 1 ? told[0] : null;
    const evs = (one?.events || m.events).filter((e) => e.rule === id);
    const fired = evs.filter((e) => e.what === "fired").sort((a, b) => a.ts - b.ts);
    const resolved = evs.filter((e) => e.what === "resolved").sort((a, b) => a.ts - b.ts);
    const lens = fired.map((f) => { const r2 = resolved.find((e) => e.ts > f.ts); return r2 ? r2.ts - f.ts : null; }).filter((v) => v != null);
    const flaps = lens.filter((v) => v < 120e3).length;
    const kp = (label, val, sub, attrs = {}) => h("div", { class: "card kpi al-kpi", ...attrs }, h("div", { class: "lbl" }, label), h("div", { class: "val" }, val), sub ? h("div", { class: "delta" }, sub) : null);
    const ed = () => editRule(rule, { after: load });
    clear(kpis,
      kp("Owner", rule.owner || h("span", { class: "muted" }, "No owner"), rule.owner ? null : can("admin") ? h("button", { class: "al-link", onclick: ed }, "Set an owner") : "the rule names none"),
      kp("Runbook", (() => { const rb = runbookOf({ ...rule, id }); return rb ? h("a", { href: rb.url, target: "_blank", rel: "noopener", title: rb.own ? rb.url : `The server's runbook: ${rb.title}` }, rb.title, " ", icon("ext")) : h("span", { class: "muted" }, "No runbook"); })(), rule.description ? h("span", { title: rule.description }, rule.description) : rule.runbook ? null : builtInRunbook(id) ? "built in" : can("admin") ? h("button", { class: "al-link", onclick: ed }, "Add one") : null),
      kp("Notified", told.length > 1 ? told.map((rt) => rt.name).join(", ") : route ? (route.legacy ? "the webhook" : route.name) : h("span", { class: "muted" }, "Nobody"),
        told.length > 1 ? h("span", { title: "Each route's last delivery, of any alert" }, told.map((rt, i) => [i ? " · " : "", failing(rt) ? h("span", { class: "al-bad", title: rt.last_error }, `${rt.name}: last send failed`) : rt.last_ok_ms ? `${rt.name} delivered ${hm(rt.last_ok_ms)}` : `${rt.name}: none sent since this server started`]))
        : !route ? (rule.route ? `route ${rule.route} does not exist` : m.routes.length ? `no route takes ${rule.severity}` : "no route: add one in Routes") : route.legacy ? "--alert-webhook; delivery is not tracked by this server"
          : failing(route) ? h("span", { class: "al-bad" }, `last send failed: ${route.last_error}`)
            : route.last_ok_ms ? h("span", { class: "al-good", title: `${fmt.n(route.sent || 0)} sent since this server started` }, `delivered ${when(route.last_ok_ms)}`) : "none sent since this server started"),
      kp(`Noise${one ? " (7 days)" : " (in memory)"}`, `${fmt.plural(fired.length, "fire")} · ${fmt.plural(flaps, "flap")}`, lens.length ? `median ${dur(median(lens))} firing` : fired.length ? "still firing" : "it did not fire"));
  }

  async function loadChart() {
    if (!rule) return;
    const now = Date.now(), at = since(), w = WINDOWS.find(([k]) => k === win)?.[2] || 3600e3;
    const rr = range();
    const from = win === "range" ? rr.from : win === "7d" ? now - w : at - w, to = win === "range" ? rr.to : win === "7d" ? now : Math.min(now, at + w);
    chartSub.textContent = `${rule.metric}${rule.eval && rule.eval !== "all" ? `, ${EVAL[rule.eval]} ${dur((rule.window_s || 300) * 1000)} (red) and raw points (grey)` : ""} · ${hm(from)} – ${to >= now - 5e3 ? "now" : hm(to)}`;
    const [raw, bt, evs] = await Promise.all([
      seriesOf(rule.metric, from, to),
      rule.sql ? null : api("/v1/alerts/backtest", { body: { rule: cleanRule(rule), from: Math.round(from), to: Math.round(to) } }).catch(() => null),
      api(`/v1/events?from=${Math.round(from)}&to=${Math.round(to)}&limit=500`).then((j) => j.events || []).catch(() => null),
    ]);
    const ev = bt?.points ? bt.points.map(btPoint) : evaluate(raw.pts, rule);
    const spans = bt?.fired ? bt.fired.map((f) => [f.from_ms, f.to_ms]) : firings(ev, rule);
    const markers = (evs || []).filter((e) => !dupChange(e)).map((e) => ({ t: e.ts_ms, label: nameUrls(e.title || e.kind), tone: TONE[e.kind] || "accent", prio: e.where?.rule === id ? 0 : e.kind === "change" ? 2 : 1 }));
    if (!evs) for (const e of m.events.filter((e2) => e2.rule === id)) markers.push({ t: e.ts, label: e.what === "fired" ? "fired" : "resolved", tone: e.what === "fired" ? "bad" : "good" });
    const series = [];
    const perCopy = (bt?.copies || []).filter((c) => c.points?.length);
    if (perCopy.length) {
      // An each-copy rule (NEW-11): the copies it fired on, or came closest on, each its own line.
      perCopy.forEach((c, i) => series.push({ name: `${c.short || copyName(c.copy)}${c.fired?.length ? ` (fired ${c.fired.length}x)` : ""}`, cls: `cp c${i % 6}`, pts: c.points.map(btPoint) }));
    } else {
      if (rule.eval && rule.eval !== "all" && raw.has) series.push({ name: "raw points", cls: "raw", pts: raw.pts });
      series.push({ name: rule.eval && rule.eval !== "all" ? `${EVAL[rule.eval]} ${dur((rule.window_s || 300) * 1000)}` : rule.metric, cls: "eval", pts: ev });
    }
    chart.update({ t0: from, t1: to, series, thr: Number(rule.threshold), resolve: rule.resolve != null ? Number(rule.resolve) : null, spans, markers, fmtv: (v) => fvr(rule, v) });
    const vals = ev.filter((p) => p[1] != null && p[0] >= at);
    const peak = vals.reduce((a, p) => (!a || (rule.op.startsWith(">") ? p[1] > a[1] : p[1] < a[1]) ? p : a), null);
    const pk = line.querySelector(".al-peak");
    if (pk) pk.textContent = peak ? ` · ${rule.op.startsWith(">") ? "peak" : "lowest"} ${fvr(rule, peak[1])} at ${hm(peak[0])}` : "";
    const hasEval = ev.some((p) => p[1] != null) || perCopy.length > 0;
    const missedCopies = (bt?.copies || []).filter((c) => c.error);
    clear(chartNote, perCopy.length
      ? [`Each copy's ${rule.eval && rule.eval !== "all" ? `${EVAL[rule.eval]} ${dur((rule.window_s || 300) * 1000)}` : "value"}, as the copies evaluated it: the ${fmt.plural(perCopy.length, "copy", "copies")} that fired most or came closest, of ${fmt.n(bt.answered)} that answered.`,
        missedCopies.length ? ` ${missedCopies.slice(0, 3).map((c) => c.short || copyName(c.copy)).join(", ")} did not answer (${nameUrls(missedCopies[0].error)}).` : "",
        bt.covered_from && bt.covered_from > from + 60e3 ? ` The copies keep points from ${hm(bt.covered_from)}.` : ""]
      : !hasEval
      ? (rule.scope === "each_copy" || METRICS[rule.metric]?.[2] === "copy" ? `This server's points have no ${rule.metric}: it lives on the copies. Their values are in Affected copies.` : rule.builtin ? `${METRICS[rule.metric]?.[0] || rule.metric} is counted from the cluster state, not kept as points: the shaded span is when it fired.` : `No ${rule.metric} values in this time.`)
      : [bt?.source === "events" ? "Replayed from the coordinator's events (copies down and up, shards losing their majority)." : bt?.source ? `Evaluated by the server from ${bt.source === "load" ? "the per-minute load table" : "its memory"}.` : `Evaluated here from ${raw.source === "load" ? "per-minute points" : "the 2-s points"}.`, evs ? " Markers: events of the cluster (failovers, rollouts, changes, notes)." : " Markers: this rule's own events (the server keeps no event log)."]);
    drawBlast(raw, at);
    drawChanged(evs, at);
  }

  async function drawBlast(raw, at) {
    // Queries and failures since it fired, from the coordinator's own points.
    const all = (state.points || []).length && at > (state.points[0]?.t || Infinity) ? state.points.filter((p) => p.t >= at) : null;
    let q = 0, bad = 0, cli = 0, rej = 0;
    const src = all || (await api(`/v1/timeseries?from=${Math.round(at)}&to=${Date.now()}`).then((j) => j.points || []).catch(() => []));
    for (const p of src) {
      if (p.t < at) continue;
      const dt = p.secs || (p.interval_ms || 2000) / 1000;
      if (p.queries != null) { q += p.queries; bad += p.query_errors || 0; cli += (p.client_errors || 0) + (p.client_deadlines || 0); }
      else { q += (p.qps || 0) * dt; bad += (p.errors_s || 0) * dt; cli += ((p.client_errors_s ?? p.http_4xx_s ?? 0) + (p.client_deadlines_s || 0)) * dt; }
      rej += (p.rejected_s || 0) * dt;
    }
    const shards = (state.cluster?.shards || []);
    const hurt = shards.filter((sh) => (sh.state && sh.state !== "ok") || (!sh.state && sh.status && sh.status !== "ok"));
    const row = (k, v, sub) => [h("dt", {}, k), h("dd", {}, v, sub ? h("div", { class: "tiny muted" }, sub) : null)];
    const kv = h("dl", { class: "kv al-blast" },
      row("Queries", q ? fmt.n(Math.round(q)) : "–", q ? `coordinator's own, since ${hm(at)}` : "no points for this time"),
      row("Failed (server)", q ? h("span", { class: bad / q > 0.01 ? "al-bad" : "" }, `${fmt.n(Math.round(bad))} (${fmt.pct(bad / q)})`) : "–", "errors and server timeouts"),
      row("Client errors", q ? `${fmt.n(Math.round(cli))}` : "–", "400s: bad SQL, the client's own deadline; not this incident's"),
      rej ? row("Refused (429)", fmt.n(Math.round(rej))) : null,
      shards.length ? row("Shards", hurt.length ? h("span", { class: "al-bad" }, `${hurt.length} of ${shards.length}: ${hurt.slice(0, 4).map((sh) => `shard ${sh.shard}${sh.state ? " " + sh.state.replace("_", " ") : ""}`).join(", ")}`) : `all ${shards.length} serve reads and writes`, hurt[0]?.why || null) : null);
    clear(blast, kv, h("div", { class: "small muted", style: { marginTop: "6px" } }, "Clients and statements: see Failing statements."));
  }

  function drawChanged(evs, at) {
    if (!evs) { clear(changed, h("div", { class: "small muted" }, "This server keeps no event log (failovers, rollouts, changes). A newer chloedb-server records them at /v1/events.")); return; }
    // A resolved alert: what changed up to 5 min after it resolved, not since.
    const end = x?.state === "ok" && x.since_ms > at ? x.since_ms + 5 * 60e3 : Infinity;
    changedSub.textContent = end < Infinity ? "from 30 min before it fired to 5 min after it resolved" : "events from 30 min before it fired";
    const list = evs.filter((e) => e.ts_ms >= at - 30 * 60e3 && e.ts_ms <= end && !e.kind?.startsWith("alert_") && !dupChange(e) && !(e.kind === "note" && e.where?.rule)).sort((a, b) => b.ts_ms - a.ts_ms).slice(0, 12);
    clear(changed, list.length ? h("div", { class: "al-changes" }, list.map((e) => h("div", { class: "al-chg" },
      h("span", { class: "al-time", title: whenTitle(e.ts_ms) }, hm(e.ts_ms)),
      h("div", {}, h("div", {}, h("span", { class: "al-dot " + (TONE[e.kind] || "") }), nameUrls(e.title || e.kind)), h("div", { class: "tiny muted" }, [e.who ? `by ${e.who}` : null, e.where?.copy ? copyName(e.where.copy) : e.where?.shard != null ? `shard ${e.where.shard}` : null, e.until_ms ? `until ${hm(e.until_ms)}` : null].filter(Boolean).join(" · ")))))) : h("div", { class: "small muted" }, "Nothing changed in the 30 minutes before it fired, or since: no failover, rollout, setting or note."));
  }

  async function loadStatements() {
    const at = since();
    try {
      const j = await api(`/v1/queries/stats?from=${Math.round(at)}&to=${Date.now()}`);
      const list = Array.isArray(j) ? j : j.statements || [];
      const errs = (x2) => x2.outcomes ? (x2.outcomes.error || 0) + (x2.outcomes.timeout || 0) : (x2.errors || 0);
      const failing = list.filter((x2) => errs(x2) > 0).sort((a, b) => errs(b) - errs(a)).slice(0, 6);
      const windowed = Array.isArray(j) || j.from_ms != null || j.from != null;
      clear(stmts, failing.length ? table([
        { key: "sql", label: "statement", render: (x2) => h("a", { class: "mono small al-sql", href: link("statement/" + encodeURIComponent(x2.fingerprint)), title: x2.sql || x2.example || x2.fingerprint }, (x2.sql || x2.example || x2.fingerprint).slice(0, 140)) },
        { key: "f", label: "failed", num: true, sort: errs, render: (x2) => h("b", {}, fmt.si(errs(x2))) },
        { key: "p99_ms", label: "p99", num: true, render: (x2) => h("span", { class: "nowrap" }, fmt.ms(x2.p99_ms)) },
      ], { rows: failing, compact: true }) : h("div", { class: "small muted", style: { padding: "4px 14px 12px" } }, `No statement failed on the server's side${windowed ? " since it fired" : ""}.`),
      windowed ? null : h("div", { class: "tiny muted", style: { padding: "0 14px 10px" } }, "Counted since this server started, client errors (bad SQL) included: it keeps no per-window statements."));
    } catch (e) { clear(stmts, h("div", { class: "small muted", style: { padding: "4px 14px 12px" } }, e instanceof ApiError && e.status === 403 ? "Statements need an admin key." : "No statements from this server.")); }
  }

  function drawCopies() {
    let rows = (x.where || []).slice().sort((a, b) => (RANK[stateName(a.state)] ?? 5) - (RANK[stateName(b.state)] ?? 5));
    let cause = false;
    if (!rows.length && x.state !== "ok" && x.state !== "disabled") {
      // A rule of this server's own points has no copies; the copies of the shards that are not ok are the likely cause.
      const bad = (state.cluster?.shards || []).filter((sh) => (sh.state && sh.state !== "ok") || (!sh.state && sh.status && sh.status !== "ok")).slice(0, 4);
      rows = bad.flatMap((sh) => (sh.replicas || []).map((rp, i) => ({ copy: rp.url, short: copyName(rp.url), state: rp.status === "down" || rp.role === "unknown" ? "firing" : "ok", value: null, since_ms: rp.down_since_ms || null })));
      cause = rows.length > 0;
    }
    const whys = [];
    rows = rows.flatMap((c) => {
      if (c.why || c.error) whys.push(`${c.short || c.name || ""}: ${c.why || c.error}`);
      if (c.copy || c.shard == null) return [c];
      const sh = (state.cluster?.shards || []).find((y, i) => (y.shard ?? i) === c.shard);
      if (!sh?.replicas?.length) return [c];
      return sh.replicas.map((rp) => ({ copy: rp.url, short: copyName(rp.url), shard: c.shard, state: rp.status === "down" || rp.role === "unknown" ? "firing" : "ok", value: null, since_ms: rp.status === "down" ? (rp.down_since_ms || c.since_ms || x.since_ms) : null, viaShard: true }));
    });
    if (rows.some((c) => c.viaShard)) cause = true;
    const shardsOf = new Set(rows.filter((c) => c.shard != null).map((c) => c.shard));
    const lines = rows.slice(0, 12).map((c) => {
      const url = c.copy, cp = copyOf(url || c.short), name = whereName(c);
      if (cp) shardsOf.add(cp.shard);
      const node = cp?.r?.name || cp?.r?.detail?.log_store?.node || null;
      const st = stateName(c.state);
      return h("tr", {},
        h("td", {}, h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "dot " + (st === "firing" ? "critical" : st === "pending" ? "pending" : st === "ok" ? "ok" : "") }), h("span", { class: "mono nowrap", title: url || "" }, name))),
        h("td", { class: "small" }, cp?.r?.role === "unknown" ? `was ${cp.r.last_role || "?"}` : cp?.r?.role || "–"),
        h("td", { class: "small nowrap" }, cause ? (st === "firing" ? h("span", { class: "al-bad" }, `down ${c.since_ms ? dur(Date.now() - c.since_ms) : ""}`) : "answers") : [fvr(rule, c.value), c.calls != null ? h("span", { class: "muted" }, ` of ${fmt.n(c.calls)} calls`) : null, c.since_ms ? h("span", { class: "muted" }, ` · ${dur(Date.now() - c.since_ms)}`) : null,
          c.rule_differs ? h("div", { class: "tiny al-warn", title: "The copy has not taken the rule's last edit: the coordinator judges its value by the rule as it is now" }, `old rule; the copy says ${stateName(c.copy_state)}`) : null]),
        h("td", {}, h("div", { class: "row nw al-cpacts", style: { gap: "2px", justifyContent: "flex-end" } },
          h("a", { class: "btn xs ghost", title: `${name}'s log lines`, href: link("logs", { q: node ? `server:${node}` : "", from: Math.round(since() - 5 * 60e3), to: Date.now() }) }, icon("logs")),
          url ? h("a", { class: "btn xs ghost", title: `Profile ${name}`, href: link("profile", { tab: "cpu", node: url }) }, icon("flame")) : null,
          cp ? h("a", { class: "btn xs ghost", title: `${name} in Cluster`, href: link("cluster", { shard: cp.shard, copy: name }) }, icon("box")) : null)));
    });
    const self = !rows.length;
    clear(copies, self
      ? h("div", { style: { padding: "2px 14px 12px" } }, h("div", { class: "small" }, rule.scope === "each_copy" ? "No copy reports it now." : `Evaluated on ${scopeText(rule)[0]}.`),
        h("div", { class: "row", style: { gap: "6px", marginTop: "8px" } },
          h("a", { class: "btn sm", href: link("logs", { q: "level>=warn", from: Math.round(since() - 5 * 60e3), to: Date.now() }) }, icon("logs"), "Its log lines"),
          h("a", { class: "btn sm", href: link("profile", { tab: "cpu" }) }, icon("flame"), "Profile it"),
          h("a", { class: "btn sm", href: link("queries") }, icon("queries"), "Running queries")))
      : [h("div", { class: "tbl-wrap" }, h("table", { class: "t compact" }, h("thead", {}, h("tr", {}, ["copy", "role", cause ? "state" : "value · for", ""].map((k) => h("th", {}, k)))), h("tbody", {}, lines))),
        whys.length ? h("div", { class: "small", style: { padding: "8px 14px 0" } }, whys.slice(0, 3).map((t) => h("div", {}, nameUrls(t)))) : null,
        cause && !whys.length ? h("div", { class: "tiny muted", style: { padding: "6px 14px 0" } }, rows.some((c) => c.viaShard) ? "The copies of the shards it fires on." : `The rule reads ${scopeText(rule)[0]}'s own points; these are the copies of the shards that are not ok.`) : null,
        rows.length > 12 ? h("div", { class: "tiny muted", style: { padding: "4px 14px" } }, `and ${rows.length - 12} more`) : null,
        h("div", { class: "row al-mitigate" }, h("span", { class: "small muted" }, "Mitigate:"),
          [...shardsOf].slice(0, 2).map((sh) => h("a", { class: "btn sm", href: link("cluster", { shard: sh }) }, `Shard ${sh} in Cluster`)),
          h("a", { class: "btn sm", href: link("queries") }, "Running queries"))]);
  }

  async function loadLogs() {
    const at = since() - 5 * 60e3, to = Date.now();
    logsLink.href = link("logs", { q: "level>=warn", from: Math.round(at), to });
    const sql = `SELECT ts, node, level, message, fields FROM logs WHERE ts >= '${new Date(at).toISOString()}' AND level IN ('error', 'warn') ORDER BY ts DESC LIMIT 2000`;
    try {
      const j = await api("/v1/logs/query", { body: { sql } });
      const cols = j.columns || [];
      const rows = (j.rows || []).map((rw) => Object.fromEntries(cols.map((c, i) => [c, rw[i]])));
      const groups = new Map();
      for (const rw of rows) {
        const key = rw.level + " " + pattern(rw.message, rw.fields);
        const g = groups.get(key) || { level: rw.level, text: pattern(rw.message, rw.fields), n: 0, nodes: new Set(), last: 0 };
        g.n++; g.nodes.add(rw.node); g.last = Math.max(g.last, Date.parse(rw.ts) || 0);
        groups.set(key, g);
      }
      const list = [...groups.values()].sort((a, b) => b.n - a.n).slice(0, 8);
      const missed = (j.nodes || []).filter((n) => n.error);
      clear(logsBox, list.length ? table([
        { key: "n", label: "lines", num: true, render: (g) => h("b", {}, fmt.si(g.n)) },
        { key: "level", label: "level", render: (g) => h("span", { class: "al-lv al-lv-" + g.level }, g.level) },
        { key: "text", label: "message pattern", render: (g) => h("a", { class: "mono small al-pat", href: link("logs", { q: `level:${g.level} "${g.text.split(" <")[0].slice(0, 60)}"`, from: Math.round(at), to }), title: g.text }, g.text) },
        { key: "nodes", label: "where", render: (g) => { const ns = [...g.nodes].map(nodeShort).sort(); return h("span", { class: "small", title: [...g.nodes].join(", ") }, ns.slice(0, 3).join(", ") + (ns.length > 3 ? ` +${ns.length - 3}` : "")); } },
      ], { rows: list, compact: true }) : h("div", { class: "small muted", style: { padding: "4px 14px 12px" } }, "No warn or error lines since it fired."),
      h("div", { class: "tiny muted", style: { padding: "4px 14px 10px" } }, `${rows.length >= 2000 ? "Patterns of the newest 2,000 lines" : `${fmt.plural(rows.length, "line")}`} from ${fmt.plural((j.nodes || []).length - missed.length, "server")}${missed.length ? `; ${missed.length} did not answer (${missed.slice(0, 3).map((n) => n.url ? copyName(n.url) : nodeShort(n.node)).join(", ")})` : ""}.`));
    } catch (e) {
      clear(logsBox, h("div", { class: "small muted", style: { padding: "4px 14px 12px" } }, e instanceof ApiError && e.status === 403 ? "Log lines need an admin key." : "No log store on this server: its lines are not kept."));
    }
  }

  function drawActivity() {
    const evs = (one?.events || m.events.filter((e) => e.rule === id)).filter((e) => e.rule === id || !e.rule);
    const notes = (one?.notes || x.notes || []).map((n) => ({ ts: n.at_ms, what: "note", by: n.by, text: n.text }));
    const items = [...evs, ...notes];
    if (x.ack && !items.some((e) => e.what === "acked")) items.push({ ts: x.ack.at_ms, what: "acked", by: x.ack.by, text: x.ack.note });
    if (x.silenced && !items.some((e) => e.what === "silenced")) items.push({ ts: x.silenced.at_ms || x.silenced.until_ms, what: "silenced", by: x.silenced.by, text: `until ${hm(x.silenced.until_ms)}${x.silenced.reason ? `: "${x.silenced.reason}"` : ""}` });
    items.sort((a, b) => (b.ts || 0) - (a.ts || 0));
    for (let i = items.length - 1; i > 0; i--) if (items[i].what === items[i - 1].what && Math.abs((items[i].ts || 0) - (items[i - 1].ts || 0)) < 2000) { items[i - 1].text ||= items[i].text; items[i - 1].value ??= items[i].value; items[i - 1].by ||= items[i].by; items.splice(i, 1); }
    const label = { fired: "Fired", resolved: "Resolved", acked: "Acknowledged", silenced: "Silenced", note: "Note", changed: "Changed" };
    clear(activity, items.length ? h("div", { class: "timeline al-activity" }, items.slice(0, 30).map((e) => h("div", { class: "ev " + (e.what === "fired" ? "firing" : e.what === "resolved" || e.what === "acked" ? "ok" : "") },
      h("div", { class: "row nw", style: { gap: "8px" } }, h("b", {}, label[e.what] || e.what), e.by ? h("span", { class: "small dim" }, e.by) : null, h("span", { class: "sep" }), h("span", { class: "tiny muted nowrap", title: e.ts ? whenTitle(e.ts) : "" }, when(e.ts))),
      e.text ? h("div", { class: "small dim" }, e.text) : e.value != null ? h("div", { class: "small dim" }, `${rule.metric} ${fvr(rule, e.value)}`) : null)))
      : h("div", { class: "small muted" }, m.fleet ? "Nothing yet." : "This server keeps the last 500 changes of all rules in memory; none is this rule's."));
    notesAt = items.length;
  }
  void notesAt;

  async function post() {
    const text = noteIn.value.trim();
    if (!text) return;
    noteBtn.disabled = true;
    try { await api(`/v1/alerts/${encodeURIComponent(id)}/notes`, { body: { text } }); noteIn.value = ""; toast("Note added", "ok"); await load(); }
    catch (e) { toast(failMsg(e, "Adding a note"), "bad"); }
    noteBtn.disabled = false;
  }
  noteBtn.onclick = post;
  noteIn.onkeydown = (e) => { if (e.key === "Enter") post(); };

  hubParams = (r2) => { const w = r2.params.get("w"); if (w && w !== win) { win = w; winSeg.set(w); loadChart(); } };
  (async () => { await load(); if (!rule) return; loadChart(); loadStatements(); loadLogs(); })();
  const offs = [bus.on("whoami", () => { if (blocked || !rule) refreshAlerts().then(() => { m = null; return load(); }); }), bus.on("alerts", () => { if (!state.paused && rule && !blocked) { m = model(state.alerts); x = { ...x, ...(m.st.get(id) || {}) }; x.silent = !!(x.silenced && (!x.silenced.until_ms || x.silenced.until_ms > Date.now())); if (!rule.enabled) x.state = "disabled"; drawHead(); drawCopies(); } })];
  const stop = every(30000, async () => { if (state.paused || !rule) return; await load(); loadChart(); loadStatements(); loadLogs(); }, false);
  return () => { offs.forEach((f) => f()); stop(); hubParams = null; };
}

// ---------------------------------------------------------------- the rule editor
function editRule(rule, opts = {}) {
  const m = model(state.alerts);
  const isNew = !rule;
  const fleet = !!m?.fleet;
  const x = rule ? { ...rule } : { id: "rule-" + Math.random().toString(36).slice(2, 8), name: "", metric: "error_ratio", op: ">", threshold: 0.02, for_s: 0, severity: "warning", enabled: true, description: "", eval: "mean", window_s: 300, scope: "self" };
  if (!fleet && !rule) { x.eval = undefined; x.window_s = undefined; x.for_s = 60; }
  if (!rule && opts.sql) { x.sql = opts.sql; x.name = opts.name || ""; x.threshold = opts.threshold || 0; x.op = ">"; x.for_s = 0; x.eval = "max"; x.window_s = 300; x.metric = "sql_" + x.id; }
  const saved = rule ? { ...rule } : null;
  const adm = can("admin");
  const locked = !!x.builtin; // built-in rules change only in severity, on/off and route
  const dis = (k) => !adm || (locked && !["severity", "enabled", "route"].includes(k)) || (!fleet && ["eval", "window_s", "resolve", "scope", "owner", "runbook", "route", "vs"].includes(k));
  const metrics = [...new Set([...Object.keys(METRICS), ...((state.points || []).slice(-3).flatMap((p) => Object.keys(p).filter((k) => typeof p[k] === "number" && k !== "t" && k !== "secs"))), x.metric])].sort();
  const name = h("input", { class: "input", value: x.name, placeholder: "Query errors over 2%", disabled: dis("name") });
  const metric = h("select", { class: "select", disabled: dis("metric"), "aria-label": "Metric" }, metrics.map((k) => h("option", { value: k, selected: k === x.metric }, METRICS[k] && !isNarrow() ? `${k} · ${METRICS[k][0]}${METRICS[k][2] === "copy" ? " (copies only)" : ""}` : k)));
  const metricHint = h("span", { class: "tiny muted al-mhint" });
  // A query in place of a metric: its first number, every 30 s, with :since and :now the window's ends.
  const sqlIn = h("textarea", { class: "input mono", rows: 3, placeholder: "Or watch a query: SELECT COUNT(*) FROM orders WHERE status = 'failed' AND ts > :since", disabled: dis("metric"), style: { fontSize: "12.5px" } });
  sqlIn.value = x.sql || "";
  // A rule on a query watches no metric: say so where the metric was.
  const sqlOpt = h("option", { value: "", hidden: true }, "the query below");
  metric.prepend(sqlOpt);
  const syncSql = () => {
    const on = !!sqlIn.value.trim();
    metric.disabled = dis("metric") || on;
    if (on) sqlOpt.selected = true;
    else if (sqlOpt.selected) metric.value = metrics.includes(x.metric) && !/^sql_/.test(x.metric) ? x.metric : metrics.find((k) => !/^sql_/.test(k)) || "";
  };
  sqlIn.addEventListener("input", syncSql);
  setTimeout(syncSql, 0);
  const hintMetric = () => { const d = METRICS[x.metric]; metricHint.textContent = d ? `${d[0]}${d[2] === "copy" ? " (copies only)" : ""} · unit: ${unitOf(x.metric) === "pct" ? "%" : unitOf(x.metric)}` : "a value of the server's points"; };
  hintMetric();
  const scopeSeg = seg(single() ? [["self", "this server"], ["each_copy", "this server's copy", "On one server, its own copy: the same points"]] : [["self", coordinator() ? "this coordinator" : "this server"], ["each_copy", `each copy (${fmt.n(allCopies())})`]], x.scope || "self", (v) => { x.scope = v; run(); }, { "aria-label": "Evaluated on" });
  const evalSeg = seg(Object.entries(EVAL).map(([k, l]) => [k, l]), x.eval || "all", (v) => { x.eval = v; run(); }, { "aria-label": "Value" });
  // The window keeps its seconds: a 10 s or 30 s window must not round to a minute on save.
  const w0 = x.window_s || 300, inMin = w0 % 60 === 0;
  const win = h("input", { class: "input", type: "number", min: 1, step: 1, value: inMin ? w0 / 60 : w0, style: { width: "80px" }, disabled: dis("window_s"), "aria-label": "Window" });
  const winUnit = h("select", { class: "select", disabled: dis("window_s"), "aria-label": "Window unit", style: { width: "auto" } }, h("option", { value: "60", selected: inMin }, "min"), h("option", { value: "1", selected: !inMin }, "s"));
  const opSeg = seg(OPS.map((o) => [o, o]), x.op, (v) => { x.op = v; run(); }, { "aria-label": "Fires when the value is" });
  // Against the threshold, or against the same window just before (S4-5): then the threshold is a factor.
  const pct = () => unitOf(x.metric) === "pct" && x.vs !== "before";
  const vsSeg = seg([["", "a threshold"], ["before", "the window before"]], x.vs || "", (v) => { const was = pct(); read(); x.vs = v || undefined; if (pct() !== was) { thr.value = toInput(x.threshold); res.value = toInput(x.resolve); } if (v === "before" && !(Number(x.threshold) > 1)) { x.threshold = 2; x.resolve = 1.5; thr.value = 2; res.value = 1.5; } run(); }, { "aria-label": "Compared with" });
  const unit = () => x.vs === "before" ? "times" : pct() ? "%" : unitOf(x.metric) === "ms" ? "ms" : unitOf(x.metric) === "/s" ? "/s" : unitOf(x.metric) === "h" ? "h" : "";
  const toInput = (v) => v == null || v === "" ? "" : pct() ? +(v * 100).toPrecision(6) : v;
  const fromInput = (v) => v === "" ? null : pct() ? Number(v) / 100 : Number(v);
  const thr = h("input", { class: "input", type: "number", step: "any", value: toInput(x.threshold), style: { width: "120px" }, disabled: dis("threshold"), "aria-label": "Threshold" });
  const res = h("input", { class: "input", type: "number", step: "any", value: toInput(x.resolve), placeholder: "same", style: { width: "110px" }, disabled: dis("resolve"), "aria-label": "Resolves at" });
  const forS = h("input", { class: "input", type: "number", min: 0, value: x.for_s || 0, style: { width: "90px" }, disabled: dis("for_s"), "aria-label": "Held for, seconds" });
  const sevSeg = seg(SEV.map((v) => [v, v]), x.severity, (v) => { x.severity = v; }, { "aria-label": "Severity" });
  const owner = h("input", { class: "input", value: x.owner || "", placeholder: "search-platform on-call", disabled: dis("owner") });
  const runbook = h("input", { class: "input", value: x.runbook || "", placeholder: "https://wiki.example.com/runbooks/errors", disabled: dis("runbook") });
  const routeSel = h("select", { class: "select", disabled: dis("route"), "aria-label": "Route" }, h("option", { value: "" }, "every route that takes its severity"), (m?.routes || []).filter((rt) => !rt.legacy).map((rt) => h("option", { value: rt.name, selected: rt.name === x.route }, `${rt.name}${rt.severities?.length ? ` (${rt.severities.join(", ")})` : ""}`)));
  const desc = h("textarea", { class: "input", rows: 2, style: { fontFamily: "var(--sans)" }, disabled: dis("description") }); desc.value = x.description || "";
  const why = h("input", { class: "input sm", placeholder: "Why (kept in the audit log)", style: { width: "260px" } });
  const unitEls = [h("span", { class: "small muted" }), h("span", { class: "small muted" })];
  // The on/off switch, visible at the top (A8).
  const sw = onSwitch(x, !adm, (on) => { x.enabled = on; });
  for (const seg0 of [scopeSeg, evalSeg, opSeg, vsSeg]) for (const b of seg0.children) b.disabled = dis(seg0 === scopeSeg ? "scope" : seg0 === evalSeg ? "eval" : seg0 === vsSeg ? "vs" : "op");
  for (const b of sevSeg.children) b.disabled = !adm;
  // The backtest.
  let span = fleet ? "7d" : "1h";
  const spanSeg = seg([["1h", "1 h"], ["24h", "24 h"], ["7d", "7 days"]], span, (v) => { span = v; run(); }, { "aria-label": "Backtest over" });
  const btChart = alertChart(220);
  const verdict = h("div", { class: "stack al-verdict", style: { gap: "4px" } });
  const btTitle = h("h3");
  const note = (t) => h("div", { class: "row nw small muted", style: { gap: "6px" } }, icon("info"), h("span", {}, t));
  const field = (label, el, hint) => h("label", { class: "f" }, label, el, hint ? h("span", { class: "tiny muted" }, hint) : null);

  let btGen = 0;
  async function backtest(r, from, to) {
    // A query's value exists only from when its rule runs: nothing to replay.
    if (r.sql) return { pts: [], spans: [], source: "sql", server: true, note: "A rule on a query has no past to replay: the server runs the query every 30 seconds once the rule is saved, and its values show on the rule's page from then on." };
    try {
      const j = await api("/v1/alerts/backtest", { body: { rule: cleanRule(r), from: Math.round(from), to: Math.round(to) } });
      return { pts: (j.points || []).map(btPoint), spans: (j.fired || []).map((f) => [f.from_ms, f.to_ms]), source: j.source, server: true, note: j.note || j.error, coveredFrom: j.covered_from || null, copies: j.copies || null, answered: j.answered, asked: j.asked };
    } catch (e) {
      // 400: a metric the load table does not keep past the last hour; 409: no log store. Evaluate here and say why.
      const said = e instanceof ApiError && (e.status === 400 || e.status === 409);
      if (!missing(e) && !said) throw e;
      const raw = await seriesOf(r.metric, from, to);
      const ev = evaluate(raw.pts, r);
      const why = !said ? null : /load table/.test(e.message) ? `The server's per-minute table does not keep ${r.metric}, so it replays it only over its last hour (since it started).` : `The server cannot backtest this: ${e.message}`;
      return { pts: ev, spans: firings(ev, r), source: raw.source, server: false, has: raw.has, note: why && `${why} Evaluated here over ${raw.source === "load" ? "its per-minute points" : "the points in this browser's memory (up to an hour)"}.` };
    }
  }
  const run = debounce(async () => {
    const my = ++btGen;
    read();
    clear(unitEls[0], unit()); clear(unitEls[1], unit());
    const now = Date.now(), from = now - (span === "7d" ? 7 * 86400e3 : span === "24h" ? 86400e3 : 3600e3);
    const ev = x.eval || "all";
    btTitle.textContent = `${ev === "all" ? "Each point" : `The ${dur((x.window_s || 300) * 1000)} ${ev === "mean" ? "mean" : ev}`} of ${x.metric} over the last ${span === "7d" ? "7 days" : span === "24h" ? "24 hours" : "hour"}`;
    clear(verdict, h("div", { class: "small muted" }, "Backtesting…"));
    try {
      const changedRule = saved && JSON.stringify(cleanRule(saved)) !== JSON.stringify(cleanRule(x));
      const [a, b] = await Promise.all([backtest(x, from, now), changedRule ? backtest(saved, from, now).catch(() => null) : null]);
      if (my !== btGen) return;
      if (a.source === "sql") { btTitle.textContent = "A rule on a query"; clear(verdict, note(a.note)); return; }
      const fmtv = (v) => fvr(x, v);
      const perCopy = (a.copies || []).filter((c) => c.points?.length);
      const series = perCopy.length
        ? perCopy.map((c, i) => ({ name: `${c.short || copyName(c.copy)}${c.fired?.length ? ` (fired ${c.fired.length}x)` : ""}`, cls: `cp c${i % 6}`, pts: c.points.map(btPoint) }))
        : [{ name: ev === "all" ? x.metric : `${EVAL[ev]} ${dur((x.window_s || 300) * 1000)}`, cls: "eval", pts: a.pts }];
      const lens = a.spans.map(([s0, s1]) => (s1 ?? now) - s0);
      const flaps = lens.filter((v) => v < 120e3).length;
      const last = [...a.pts].reverse().find((p) => p[1] != null);
      const label = span === "7d" ? "7 days" : span === "24h" ? "24 hours" : "hour";
      // The span the backtest covered (NEW-10, A10): a server that started an hour ago cannot say a week.
      const first = a.coveredFrom || a.pts.find((p) => p[1] != null)?.[0] || null;
      const short = first && first > from + Math.max(120e3, (now - from) * 0.05);
      // The chart over what there is, not a week of nothing.
      btChart.update({ t0: short ? Math.max(from, first - (now - first) * 0.05) : from, t1: now, series, thr: Number(x.threshold), resolve: x.resolve != null ? Number(x.resolve) : null, spans: a.spans, markers: [], fmtv });
      const over = short ? `the ${dur(now - first)} it has points for (since ${hm(first)}; you asked for ${label})` : `the last ${label}`;
      const out = [];
      if (!a.pts.some((p) => p[1] != null) && !a.spans.length && a.source !== "events") out.push(h("div", { class: "row nw", style: { gap: "8px" } }, icon("info"), h("span", {}, `No values of ${x.metric} over ${short ? `the ${dur(now - first)} it has points for (since ${hm(first)})` : `the last ${label}`}${METRICS[x.metric]?.[2] === "copy" && x.scope !== "each_copy" ? ": this metric lives on the copies; evaluate it on each copy" : x.vs === "before" ? `: a comparison with the window before takes two windows of points (${dur(2 * (x.window_s || 300) * 1000)})` : ""}.`)));
      else out.push(h("div", { class: "row nw", style: { gap: "8px" } }, icon(a.spans.length ? "warn" : "okc"), h("span", {}, `Over ${over} this rule would have fired `, h("b", {}, fmt.plural(a.spans.length, "time")), a.spans.length ? ` (median ${dur(median(lens))} firing, ${fmt.plural(flaps, "flap")} under 2 min)` : "", a.source === "events" ? "." : `. Now: ${last ? fmtv(last[1]) : "no value"}${perCopy.length ? " on the copy that came closest" : ""}.`)));
      if (a.source === "copies") out.push(note(`Evaluated by each copy over its own points: ${fmt.n(a.answered ?? 0)} of ${fmt.n(a.asked ?? 0)} copies answered${perCopy.length ? `; the chart shows the ${fmt.plural(perCopy.length, "copy", "copies")} that fired most or came closest` : ""}.${(a.copies || []).some((c) => c.error) ? ` Not answered: ${(a.copies || []).filter((c) => c.error).slice(0, 3).map((c) => `${c.short || copyName(c.copy)} (${nameUrls(c.error)})`).join(", ")}.` : ""}`));
      if (a.source === "events") out.push(note(`Replayed from the coordinator's events: when copies went down and came back, and shards lost and got back their majority. The line is 1 while it held.`));
      if (b) out.push(note(`The rule as saved (${cond(saved, true)}) would have fired ${fmt.plural(b.spans.length, "time")}.`));
      if (!a.server && a.note) out.push(note(a.note));
      else if (a.source !== "copies" && a.source !== "events") out.push(note(a.server ? `Evaluated by the server over ${a.source === "load" ? "the per-minute load table (only the metrics it holds)" : "its memory (the last hour, 2-s points)"}.` : `This server cannot backtest: evaluated here over ${a.source === "load" ? "its per-minute points" : "the points in this browser's memory (up to an hour)"}.`));
      if (a.server && a.note && a.source !== "events") out.push(note(a.note));
      clear(verdict, out);
    } catch (e) { if (my === btGen) clear(verdict, h("div", { class: "error" }, e.message)); }
  }, 250);
  function read() {
    x.name = name.value.trim();
    x.sql = sqlIn.value.trim() || undefined;
    x.metric = x.sql ? "sql_" + x.id : metric.value;
    x.threshold = fromInput(thr.value) ?? x.threshold;
    const rv = fromInput(res.value);
    x.resolve = rv == null ? undefined : rv;
    x.for_s = Number(forS.value) || 0;
    if (!dis("window_s")) x.window_s = Math.max(1, Number(win.value) || (winUnit.value === "1" ? 300 : 5)) * Number(winUnit.value);
    x.owner = owner.value.trim() || undefined;
    x.runbook = runbook.value.trim() || undefined;
    x.route = routeSel.value || undefined;
    x.description = desc.value.trim();
    if (!fleet) { if (!x.sql) { delete x.eval; delete x.window_s; } delete x.resolve; delete x.scope; }
  }
  for (const el of [thr, res, forS, win]) el.oninput = run;
  winUnit.onchange = run;
  metric.onchange = () => { const before = pct(); x.metric = metric.value; if (pct() !== before) { thr.value = toInput(x.threshold); res.value = toInput(x.resolve); } hintMetric(); run(); };

  const saveBtn = h("button", { class: "btn primary", disabled: !adm, title: adm ? "" : Needs("admin") }, icon("check"), isNew ? "Create rule" : "Save");
  const more = h("button", { class: "btn ghost", title: "More", "aria-label": "More actions" }, icon("dots"));
  const d = drawer({
    title: isNew ? "New alert rule" : x.name,
    sub: isNew ? "fires when the value crosses the threshold, and resolves when it comes back past the resolve line" : `${x.id} · ${m?.st.get(x.id)?.state?.replace("_", " ") || ""}${x.owner ? ` · owned by ${x.owner}` : ""}`,
    wide: true,
    body: [
      h("div", { class: "row between al-ed-top" }, sw, locked ? h("span", { class: "small muted" }, "Built in: only severity, on/off and route change.") : null, !fleet ? h("span", { class: "small muted" }, "This server checks each point: window, resolve, scope, owner, runbook and route need a newer chloedb-server.") : null),
      h("div", { class: "grid g2 al-ed-names" }, field("Name", name), h("label", { class: "f" }, "Metric", metric, metricHint)),
      field("Or a query", sqlIn, "A SELECT whose first number is the value, run every 30 s; :since and :now are the window's start and end. Any table: orders, otel_logs, yours."),
      x.sql_run ? h("div", { class: "small " + (x.sql_run.error ? "error" : "muted") }, x.sql_run.error ? `Its last run failed: ${x.sql_run.error}` : `Last run ${fmt.ago(x.sql_run.at_ms)}: ${fmt.n(x.sql_run.value, 3)} in ${fmt.ms(x.sql_run.took_ms)}`) : null,
      h("div", { class: "row al-ed-row" }, field("Evaluated on", scopeSeg), field("Value", evalSeg), h("label", { class: "f" }, "Window", h("span", { class: "row nw", style: { gap: "4px" } }, win, winUnit)), field("Compared with", vsSeg, "the window before: fires when the value is the threshold times the one before")),
      h("div", { class: "row al-ed-row" }, field("Fires when the value is", opSeg), h("label", { class: "f" }, "Threshold", h("span", { class: "row nw", style: { gap: "4px" } }, thr, unitEls[0])), field("Held for, seconds", forS), field("Severity", sevSeg), h("label", { class: "f" }, "Resolves at", h("span", { class: "row nw", style: { gap: "4px" } }, res, unitEls[1]))),
      h("section", { class: "card al-edbt", style: { marginTop: "4px" } }, h("div", { class: "hd" }, btTitle, h("span", { class: "sub" }, "shaded: when it would have fired"), h("div", { class: "tools" }, spanSeg)), h("div", { class: "bd" }, btChart, verdict)),
      h("div", { class: "grid g2" }, field("Owner", owner), field("Runbook", runbook)),
      h("div", { class: "grid g2" }, field("Route", routeSel, "who is told: routes are set in Routes"), field("Description (sent with the message)", desc)),
    ],
    foot: [more, h("span", { class: "sep" }), adm ? why : null, h("button", { class: "btn", onclick: () => d.close() }, "Cancel"), saveBtn],
  });
  run();
  more.onclick = () => menu(more, [
    { label: "Copy as JSON", icon: "copy", onclick: () => { read(); copy(JSON.stringify(cleanRule(x), null, 2), "Rule copied as JSON"); } },
    isNew ? null : { label: "Open its alert page", icon: "alerts", onclick: () => { d.close(); location.hash = alertLink(x.id); } },
    !isNew && !locked && adm ? "-" : null,
    !isNew && !locked && adm ? { label: "Delete this rule", icon: "trash", danger: true, onclick: del } : null,
  ].filter(Boolean));
  async function del() {
    const st = m?.st.get(x.id)?.state;
    const ok = await ask({ title: `Delete "${x.name}"?`, body: `The server stops checking it${st === "firing" ? " and sends a RESOLVED message (it is firing now)" : ""}. Its past events stay. To stop it for a while, turn it off or silence it instead.`, ok: "Delete", danger: true, typed: x.id });
    if (!ok) return;
    try { await saveRules((model(state.alerts)?.rules || []).filter((y) => y.id !== x.id), ok.reason); toast("Rule deleted", "ok"); d.close(); opts.after?.(); } catch { /* told */ }
  }
  saveBtn.onclick = async () => {
    read();
    if (!x.name) x.name = `${x.metric} ${x.op} ${fv(x.metric, x.threshold)}`;
    const cur = model(state.alerts)?.rules || [];
    const next = isNew ? [...cur, x] : cur.map((y) => y.id === x.id ? x : y);
    try { const j = await saveRules(next, why.value.trim()); toast(...(pushedText(j, isNew ? "Rule created" : "Rule saved") || [isNew ? "Rule created" : "Rule saved", "ok"])); d.close(); opts.after?.(); } catch { /* told */ }
  };
}

// ---------------------------------------------------------------- routes
function routesDrawer() {
  const m = model(state.alerts);
  const body = h("div", { class: "stack" });
  const d = drawer({ title: "Routes", sub: "where alert messages go, by severity; each with its delivery status", wide: true, body });
  if (!m?.fleet) {
    clear(body, h("div", { class: "notice" }, m?.routes.length ? "This server posts every alert that fires or resolves to one webhook (--alert-webhook). It does not track deliveries, route by severity or send a test: a newer chloedb-server does." : "No webhook: this server tells nobody when an alert fires. Start it with --alert-webhook URL, or run a newer chloedb-server to set routes here."),
      h("p", { class: "small muted" }, "A message reads: [chloedb FIRING critical] <rule> on <cluster>/<where>: <metric> = <value> (<op> <threshold>, <eval> over <window>). <description> Runbook: <url>, then a link to the alert's page."));
    return d;
  }
  let routes = m.routes.map((r) => ({ ...r, severities: r.severities || [] }));
  let editing = false;
  const draw = () => {
    const adm = can("admin");
    const KINDS = [["webhook", "Webhook"], ["pagerduty", "PagerDuty"], ["opsgenie", "Opsgenie"]];
    const provider = (r) => r.kind === "pagerduty" || r.kind === "opsgenie";
    const rows = routes.map((r, i) => editing
      ? h("tr", {},
        h("td", {}, h("div", { class: "stack", style: { gap: "4px" } },
          h("input", { class: "input sm", value: r.name, style: { width: "120px" }, "aria-label": "Route name", oninput: (e) => { r.name = e.target.value.trim(); } }),
          h("select", { class: "select sm", "aria-label": "Kind", onchange: (e) => { r.kind = e.target.value; draw(); } }, KINDS.map(([k, l]) => h("option", { value: k, selected: (r.kind || "webhook") === k }, l))))),
        h("td", {}, h("div", { class: "stack", style: { gap: "4px" } },
          h("input", { class: "input sm mono", value: r.url || "", "aria-label": "URL", placeholder: r.shown && r.wasKind === (r.kind || "webhook") ? `${r.shown} (kept)` : provider(r) ? `${r.kind === "pagerduty" ? "events.pagerduty.com/v2/enqueue" : "api.opsgenie.com/v2/alerts"} (leave empty)` : "https://hooks.slack.com/…", title: r.shown ? "The server shows only the start of a saved URL. Leave this empty to keep it, or type a URL to replace it." : "", style: { width: "100%" }, oninput: (e) => { r.url = e.target.value.trim(); } }),
          provider(r) ? h("input", { class: "input sm mono", type: "password", autocomplete: "off", value: r.key || "", "aria-label": r.kind === "pagerduty" ? "Routing key" : "API key", placeholder: r.has_key ? "key kept: type one to replace it" : r.kind === "pagerduty" ? "the service's integration (routing) key" : "the Opsgenie API key", oninput: (e) => { r.key = e.target.value.trim(); } }) : null,
          h("label", { class: "row nw small", style: { gap: "6px" } }, "Repeat unacked criticals every", h("input", { class: "input sm", type: "number", min: 0, max: 1440, value: r.repeat_min || 0, style: { width: "70px" }, "aria-label": "Repeat minutes", oninput: (e) => { r.repeat_min = Math.max(0, Math.min(1440, Number(e.target.value) || 0)); } }), "min (0: off)"))),
        h("td", {}, h("div", { class: "row nw", style: { gap: "4px" } }, SEV.map((sv) => h("button", { class: "chip", "aria-pressed": String(!r.severities.length || r.severities.includes(sv)), onclick: (e) => { const on = e.currentTarget.getAttribute("aria-pressed") === "true"; const cur = r.severities.length ? r.severities : SEV.slice(); r.severities = on ? cur.filter((y) => y !== sv) : [...cur, sv]; if (r.severities.length === SEV.length) r.severities = []; draw(); } }, sv)))),
        h("td", { colspan: 2 }), h("td", {}, h("button", { class: "btn xs ghost", title: "Remove this route", onclick: () => { routes.splice(i, 1); draw(); } }, icon("x"))))
      : h("tr", {},
        h("td", { class: "al-ro-name" }, h("b", {}, r.name), h("div", { class: "tiny muted" }, [KINDS.find(([k]) => k === (r.kind || "webhook"))?.[1] || r.kind, r.repeat_min ? `again every ${r.repeat_min} min unacked` : null].filter(Boolean).join(" · "))),
        h("td", { class: "mono small ellipsis al-ro-url", style: { maxWidth: "280px" }, title: r.url || "" }, r.url ? r.url.replace(/^https?:\/\//, "").slice(0, 48) : "–", provider(r) ? h("div", { class: "tiny muted", style: { fontFamily: "var(--sans)" } }, r.has_key ? "key set (not shown)" : h("span", { class: "al-bad" }, "no key")) : null),
        h("td", { class: "small al-ro-sev" }, r.severities?.length ? r.severities.join(", ") : "every severity"),
        h("td", { class: "small al-ro-sent" }, `${fmt.n(r.sent || 0)} sent`, r.failed ? h("span", { class: "al-bad" }, ` · ${fmt.n(r.failed)} failed`) : null),
        h("td", { class: "small al-ro-st" }, r.last_error && (!r.last_ok_ms || (r.last_error_ms || 0) > r.last_ok_ms) ? h("span", { class: "al-bad", title: r.last_error }, `failing: ${r.last_error.slice(0, 60)}`) : r.last_ok_ms ? h("span", { class: "al-good" }, `delivered ${ago(r.last_ok_ms)}`) : h("span", { class: "muted", title: "Deliveries are counted in memory: a restart starts them again" }, "none sent since this server started")),
        h("td", { class: "al-ro-test" }, h("button", { class: "btn xs", disabled: !can("admin"), title: can("admin") ? "Send a test message through this route now" : Needs("admin"), onclick: async (e) => {
          const b = e.currentTarget; b.disabled = true;
          try { const j = await api(`/v1/alerts/routes/${encodeURIComponent(r.name)}/test`, { body: {} }); const ms = j.ms != null ? ` in ${Math.round(j.ms)} ms` : ""; toast(j.ok === false ? `Test through ${r.name} failed${ms}: ${j.error || j.status}` : `Test sent through ${r.name}${ms}${j.status ? ` (${j.status})` : ""}`, j.ok === false ? "bad" : "ok"); await refreshAlerts(); routes = model(state.alerts).routes.map((y) => ({ ...y, severities: y.severities || [] })); draw(); }
          catch (err) { toast(failMsg(err, "Testing the route"), "bad"); }
          b.disabled = false;
        } }, "Test"))));
    clear(body,
      h("div", { class: "tbl-wrap al-rotbl" }, h("table", { class: "t compact" }, h("thead", {}, h("tr", {}, ["name", "URL", "severities", "sent", "status", ""].map((k) => h("th", {}, k)))), h("tbody", {}, rows.length ? rows : h("tr", {}, h("td", { colspan: 6 }, empty("route", "No routes", "Nobody is told when an alert fires.")))))),
      h("div", { class: "row" },
        editing ? h("button", { class: "btn sm", onclick: () => { routes.push({ name: `route-${routes.length + 1}`, url: "", severities: [] }); draw(); } }, icon("plus"), "Add a route") : null,
        h("span", { class: "sep" }),
        editing ? h("button", { class: "btn sm", onclick: () => { editing = false; routes = model(state.alerts).routes.map((y) => ({ ...y, severities: y.severities || [] })); draw(); } }, "Cancel") : null,
        editing ? h("button", { class: "btn sm primary", onclick: save }, icon("check"), "Save routes")
          : h("button", { class: "btn sm", disabled: !adm, title: adm ? "" : Needs("admin"), onclick: () => { editing = true; routes = routes.map((y) => ({ ...y, shown: y.url, was: y.name, wasKind: y.kind || "webhook", url: "", key: "" })); draw(); } }, icon("edit"), "Edit routes")),
      h("p", { class: "small muted" }, "A PagerDuty route opens an incident through the Events API and its RESOLVED closes it; an Opsgenie route creates and closes an alert. Both name the alert with one key, chloedb/<cluster>/<rule>/<when it fired>, so a message sent twice (by two coordinators, or retried) makes one incident. A webhook gets the same key as dedup_key, and every try of a message carries an Idempotency-Key."),
      h("p", { class: "small muted" }, "A rule with a route is sent only there; a rule without one goes to every route that takes its severity. A message reads: [chloedb FIRING critical] <rule> on <cluster>/<where>: <metric> = <value> (<op> <threshold>, <eval> over <window>). <description> Runbook: <url>, and a link to the alert's page when the server has --public-url. A RESOLVED message follows when it resolves, also when its rule is edited or deleted. The server shows only the start of each URL."));
  };
  async function save() {
    // The server shows a saved URL cut to its host ("https://hooks.slack.com/..."): sending that back would
    // replace the real URL, so an untouched URL is sent empty, which keeps it. A renamed route needs it again.
    const prov = (r) => r.kind === "pagerduty" || r.kind === "opsgenie";
    if (routes.some((r) => !r.name || (r.url ? !/^https?:\/\//.test(r.url) : !prov(r) && (!r.shown || r.was !== r.name || r.wasKind !== (r.kind || "webhook"))))) { toast("Each webhook route needs a name and an http(s) URL (a renamed route needs its URL again)", "bad"); return; }
    if (routes.some((r) => prov(r) && !r.key && !(r.has_key && r.was === r.name))) { toast("A PagerDuty or Opsgenie route needs its key", "bad"); return; }
    const ok = await ask({ title: "Save the routes?", body: `${fmt.plural(routes.length, "route")}. Messages go through them from now on.`, ok: "Save" });
    if (!ok) return;
    try { await api("/v1/alerts/routes", { method: "PUT", body: { routes: routes.map((r) => ({ name: r.name, url: r.url, severities: r.severities, kind: r.kind || "webhook", key: r.key || "", repeat_min: Number(r.repeat_min) || 0 })) }, reason: ok.reason }); toast("Routes saved", "ok"); editing = false; await refreshAlerts(); routes = model(state.alerts).routes.map((y) => ({ ...y, severities: y.severities || [] })); draw(); }
    catch (e) { toast(failMsg(e, "Saving the routes"), "bad"); }
  }
  draw();
  return d;
}
