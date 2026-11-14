// The workload views' shared parts (Queries, Statement, Performance,
// Profiler, Query path): what a query's outcome means, who sent it, the
// statements read from whichever API the server has, the query drawer
// with the trace's fan-out summary, and the dialogs that cancel and
// contain. Shared helpers of the shell are used when the shell has them,
// with a fallback here, so that these views work on either side of it.

import * as core from "./core.js";
import * as data from "./data.js";
import { profileView } from "./profile.js";

const { h, s, icon, clear, fmt, api, card, pill, empty, sqlBlock, copy, toast, modal, seg, showTip, hideTip, tipOn } = core;
/** NEW-workload-9: a drawer closes when the page changes under it (Back, or a link inside it). */
function drawer(o) {
  const off = () => d.close();
  const d = core.drawer({ ...o, onClose: () => { removeEventListener("hashchange", off); o.onClose?.(); } });
  addEventListener("hashchange", off);
  return d;
}
export const { state } = data;

// ---------------------------------------------------------------- shell helpers, or a fallback

/** The short name of a copy: s1-0 (shard 1, copy 0), or its node name. */
export function copyName(url) {
  if (!url) return "–";
  if (core.copyName) { try { const n = core.copyName(url, state.cluster); if (n) return n; } catch { /* the fallback */ } }
  for (const sh of state.cluster?.shards || []) {
    const i = (sh.replicas || []).findIndex((r) => r.url === url || r.grpc === url);
    if (i >= 0) return sh.replicas[i].short || sh.replicas[i].name || `s${sh.shard}-${i}`;
  }
  return url.replace(/^https?:\/\//, "");
}
export const copyTitle = (url) => (core.copyTitle ? core.copyTitle(url) : url);

/** A link to a view that keeps the time range: #/view?k=v. */
export function link(view, params = {}) {
  if (core.link) return core.link(view, params);
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v != null && v !== "") q.set(k, v);
  const cur = new URLSearchParams(location.hash.split("?")[1] || "");
  for (const k of ["from", "to", "range"]) if (cur.has(k) && !q.has(k)) q.set(k, cur.get(k));
  const qs = q.toString();
  return `#/${view}${qs ? "?" + qs : ""}`;
}

/**
 * Every copy of a coordinator's cluster with its latest load, from
 * /v1/cluster: {url, name, node, zone, shard, j, role, status, has (its
 * detail answered), p99, qps, cpu (busy, 0 to 1), disk, ...}.
 */
export function copies(c = state.cluster) {
  const out = [];
  for (const sh of c?.shards || []) (sh.replicas || []).forEach((r, j) => {
    const d = r.detail || {}, ld = d.load || {}, mi = d.minute || {};
    const cores = mi.cores || d.system?.cores || ld.cores;
    out.push({
      url: r.url, name: copyName(r.url), node: r.node || null, zone: r.zone || null, shard: sh.shard, j, role: r.role, status: r.status, has: !!r.detail,
      p99: mi.p99_ms ?? ld.p99_ms, qps: mi.qps ?? ld.qps,
      cpu: mi.busy ?? (ld.cpu != null && cores ? ld.cpu / cores : null),
      disk: ld.disk_used_ratio ?? mi.disk_used_ratio, segments: ld.segments ?? r.segments, docs: ld.docs ?? r.docs,
      merge_failing: !!ld.merge_failing, flush_failing: !!ld.flush_failing, failing: !!ld.failing,
      behind: r.role === "primary" ? null : ld.replica_behind_max ?? null, out_of_sync: ld.replicas_out_of_sync, rss: ld.rss_bytes, wal: ld.wal_bytes, file: ld.file_bytes,
      deleted: ld.deleted_docs,
      // Memory: its heap, the queries' budget and the process's limit (cgroup or RAM).
      heap: ld.heap_bytes ?? null, qbudget: ld.query_memory_limit_bytes ?? null, memLimit: d.system?.memory_limit_bytes ?? null,
    });
  });
  return out;
}

/** A time: 19:42:15 today, Nov 2 19:42 another day. */
export function when(ms, o) {
  if (core.when) return core.when(ms, o);
  if (!ms) return "–";
  const d = new Date(ms), today = new Date();
  return d.toDateString() === today.toDateString() ? fmt.time(ms) : d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false });
}

/** Whether the viewer's key may: "read" | "write" | "admin", and an action's route (core.js `can`). */
export const can = (scope, route) => core.can(scope, route);
const needs = (scope, route) => core.needs(scope, route);

/** The global time range: {from, to, live, label}. */
export function timeRange() {
  if (data.timeRange) { const r = data.timeRange(); if (r) return r; }
  if (state.zoom) return { from: state.zoom[0], to: state.zoom[1], live: false, label: `${fmt.time(state.zoom[0], false)} to ${fmt.time(state.zoom[1], false)}` };
  const to = Date.now(), min = state.range || 15;
  return { from: to - min * 60e3, to, live: true, label: `last ${min >= 60 ? fmt.plural(min / 60, "hour") : min + " min"}` };
}
export const paused = () => !!state.paused;
/** The range as the API takes it: from=&to= (ms). */
export const rangeQuery = (r = timeRange()) => `from=${Math.round(r.from)}&to=${Math.round(r.to ?? Date.now())}`;

/** True on a coordinator. */
export const isCoord = () => (data.isCoordinator ? data.isCoordinator() : state.health?.role === "coordinator");

/** True on a narrow screen (a phone): tables keep their key columns and the rest opens in a sheet. */
export function narrow() {
  if (core.isNarrow) { try { return !!core.isNarrow(); } catch { /* the fallback */ } }
  return typeof matchMedia === "function" && matchMedia("(max-width: 900px)").matches;
}
// The shell emits "narrow" when the width crosses 900 px; until it does, so does this.
if (!core.isNarrow && typeof matchMedia === "function") { const mq = matchMedia("(max-width: 900px)"); mq.addEventListener?.("change", () => core.bus.emit("narrow", mq.matches)); }
/** The columns a table shows: on a narrow screen, those not marked `wide`. */
export const cols = (list) => list.filter((c) => c && !(c.wide && narrow()));
/** A sheet with every field of a row and its actions (a full-screen sheet on a phone). */
export function sheet(title, rows, actions = [], sub) {
  return drawer({ title, sub, body: h("dl", { class: "kv wl-sheet" }, rows.filter((r) => r && r[1] != null && r[1] !== "").flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
    foot: actions.filter(Boolean).length ? h("div", { class: "row", style: { gap: "6px", flexWrap: "wrap" } }, actions.filter(Boolean)) : null });
}

/** Repeats `f` while the view is shown, skipping while paused. */
export function poll(ms, f) {
  return core.every(ms, async () => { if (!paused()) await f(); });
}

/** Paused by the pause button, not by a fixed window: what is running now is "now" whatever window is picked. */
export const livePaused = () => !!state.paused && state.from == null;
/** Like `poll`, but a fixed window does not stop it (Running now). */
export function livePoll(ms, f) {
  return core.every(ms, async () => { if (!livePaused()) await f(); });
}

// ---------------------------------------------------------------- outcomes

/**
 * What a query's outcome means. A server fault is red; what a client did
 * (bad SQL, its own deadline) is not the server's failure and says so.
 */
export const OUTCOME = {
  ok: { tone: "ok", color: "var(--s1)", label: "ok", says: "answered" },
  error: { tone: "bad", color: "var(--crit)", label: "server error", says: "the server failed it", server: true },
  timeout: { tone: "bad", color: "var(--s8)", label: "server timeout", says: "the server's deadline passed", server: true },
  client_error: { tone: "warn", color: "var(--s4)", label: "client error", says: "400: the request was wrong (bad SQL, an unknown column); not a server fault" },
  client_deadline: { tone: "info", color: "var(--s6)", label: "client deadline", says: "the client's own deadline passed first; not a server fault" },
  refused: { tone: "warn", color: "var(--s2)", label: "refused", says: "429: no slot was free, or a contain rule refused it" },
  rejected: { tone: "warn", color: "var(--s2)", label: "refused", says: "429: no slot was free" },
  cancelled: { tone: "info", color: "var(--s7)", label: "cancelled", says: "an operator cancelled it (499)" },
  partial: { tone: "warn", color: "var(--s5)", label: "partial", says: "some shards did not answer: the answer is incomplete" },
};
export const outcomeOf = (o) => OUTCOME[o] || { tone: "info", color: "var(--s3)", label: o || "–", says: o || "" };
export const outcomePill = (o) => { const x = outcomeOf(o); const p = pill(x.tone, x.label); p.title = x.says; return p; };
/** The order outcomes stack in a chart and list in a split. */
export const OUTCOME_ORDER = ["error", "timeout", "client_error", "client_deadline", "refused", "rejected", "partial", "cancelled", "ok"];

/** Sums of a statement's outcomes: {calls, server, client, deadlines, refused, other}. */
export function split(outcomes, calls) {
  const o = outcomes || {};
  const server = (o.error || 0) + (o.timeout || 0);
  const client = o.client_error || 0;
  const deadlines = o.client_deadline || 0;
  const refused = (o.refused || 0) + (o.rejected || 0);
  return { calls: calls ?? Object.values(o).reduce((a, b) => a + b, 0), server, client, deadlines, refused, failed: server + client + deadlines + refused + (o.partial || 0) + (o.cancelled || 0) };
}

/** A stacked bar of a statement's outcomes, with the split in its tooltip. */
export function outcomeBar(outcomes, width = 90) {
  const o = outcomes || {}, total = Object.values(o).reduce((a, b) => a + b, 0);
  if (!total) return h("span", { class: "muted" }, "–");
  const keys = OUTCOME_ORDER.filter((k) => o[k]).concat(Object.keys(o).filter((k) => !OUTCOME_ORDER.includes(k) && o[k]));
  const bar = h("span", { class: "wl-obar", style: { width: width + "px" } }, keys.map((k) => h("i", { style: { flexGrow: o[k], background: outcomeOf(k).color } })));
  tipOn(bar, () => [h("div", { class: "t" }, `${fmt.n(total)} calls`), ...keys.map((k) => h("div", { class: "r" }, h("span", { class: "k box", style: { background: outcomeOf(k).color } }), h("b", {}, fmt.n(o[k])), h("span", { class: "n" }, `${outcomeOf(k).label} · ${fmt.pct(o[k] / total, 1)}`)))]);
  return bar;
}

// ---------------------------------------------------------------- who and with what

/** A short JSON of a query's parameters. */
export function paramsText(p, max = 80) {
  if (p == null) return "";
  let t = typeof p === "string" ? p : JSON.stringify(p);
  if (t === "{}" || t === "[]") return "";
  return t.length > max ? t.slice(0, max - 1) + "…" : t;
}
export const paramsObj = (p) => { if (p == null) return null; if (typeof p === "object") return p; try { return JSON.parse(p); } catch { return null; } };

/** Who sent a query: its key, app and address, each a chip. */
export function whoChips(q) {
  const out = [];
  if (q.key) out.push(h("span", { class: "badge mono", title: q.key_id ? `API key ${q.key} (${q.key_id})` : `API key ${q.key}` }, q.key));
  if (q.app) out.push(h("span", { class: "badge", title: "the app (x-chloedb-app, or the user agent's product)" }, q.app));
  if (q.addr) out.push(h("span", { class: "badge mono", title: "the client's address" }, q.addr));
  if (q.client === "command-center") out.push(h("span", { class: "badge accent", title: "sent by the command center itself" }, "command center"));
  return out.length ? h("span", { class: "row", style: { gap: "4px", flexWrap: "wrap" } }, out) : h("span", { class: "muted", title: "This server does not record who sent a query" }, "–");
}

/** The command center's own queries (its polling, Query path, Profiler runs). */
// A statement says whether every call of it was the command center's (command_center); its
// sample is only its latest call, so it decides only on an older server.
export const isOwn = (x) => typeof x?.command_center === "boolean" ? x.command_center : x?.client === "command-center" || x?.sample?.client === "command-center";

/** A short id for a statement's shape (its fingerprint), for titles. */
export function shortFp(fp) {
  if (!fp) return "–";
  let x = 0x811c9dc5;
  for (let i = 0; i < fp.length; i++) { x ^= fp.charCodeAt(i); x = Math.imul(x, 0x01000193) >>> 0; }
  return x.toString(16).padStart(8, "0").slice(0, 6);
}
export const statementLink = (fp, extra = {}) => link("statement/" + encodeURIComponent(fp), extra);

/** A rough fingerprint of SQL, for a server that sends none: literals as ?. */
export function roughFp(sql) {
  return (sql || "").replace(/'(?:[^']|'')*'/g, "?").replace(/\b\d+(?:\.\d+)?\b/g, "?").replace(/\s+/g, " ").trim();
}

/** The workbench and the profiler, opened on a query with its parameters. */
export const workbenchLink = (sql, params) => "#/sql?q=" + encodeURIComponent(sql) + (paramsObj(params) ? "&p=" + encodeURIComponent(JSON.stringify(paramsObj(params))) : "");
/** The profiler with a query filled in, never run on open; `fp` names its statement (its p99, its contain rules). */
export const profileLink = (sql, params, fp) => "#/profile?tab=query&sql=" + encodeURIComponent(sql) + (paramsObj(params) ? "&p=" + encodeURIComponent(JSON.stringify(paramsObj(params))) : "") + (fp ? "&fp=" + encodeURIComponent(fp) : "");
export const nodeLink = (url, tab = "cpu") => link("profile", { tab, node: url });
export const copyLink = (url) => link("cluster", { copy: url });
export const shardLink = (i) => link("cluster", { shard: i });

// ---------------------------------------------------------------- statements

/**
 * The statements of a window, from `/v1/queries/stats?from=&to=` (each with
 * the window before it in `prev`), or from an older server's counts since
 * it started. -> {statements, windowed, since_ms}
 */
export async function statements(range = timeRange()) {
  const j = await api(`/v1/queries/stats?${rangeQuery(range)}&limit=1000`);
  const list = Array.isArray(j) ? j : j.statements || [];
  const windowed = Array.isArray(j) || j.from != null || j.from_ms != null || list.some((x) => x.prev !== undefined || x.outcomes);
  const out = list.map((x) => normStatement(x));
  const total = out.reduce((a, x) => a + (x.total_ms || 0), 0) || 1;
  for (const x of out) x.share = x.share ?? x.total_ms / total;
  return { statements: out, windowed, since_ms: j.since_ms ?? null, from: range.from, to: range.to, coordinators: j.coordinators || null, ...held(j, range, windowed) };
}

/**
 * What of the range a server holds statements for: it keeps them from
 * `held_from_ms` (when it started, or the oldest minute it keeps), so a
 * rate is over the held part, and the window before is only compared
 * when it is held too. -> {held_from_ms, span_ms, partial, prevHeld}
 */
export function held(j, range = timeRange(), windowed = true) {
  const to = range.to ?? Date.now();
  const from = windowed ? range.from : (j.since_ms ?? range.from);
  const hf = j.held_from_ms ?? (windowed ? null : j.since_ms) ?? null;
  const start = hf != null ? Math.max(from, hf) : from;
  const prevFrom = j.prev?.from ?? (from - (to - from));
  return { held_from_ms: hf, span_ms: Math.max(1000, to - start), partial: hf != null && hf > from + 60e3, prevHeld: hf == null || hf <= prevFrom + 60e3 };
}

/** Whether a statement is new in its window: the server's word when it gives one. */
export const isNewStatement = (x, windowMs) => typeof x.new === "boolean" ? x.new : !!(x.first_seen_ms && windowMs && x.first_seen_ms >= Date.now() - windowMs);

/** One statement in the shape the contract gives, whichever server answered. */
export function normStatement(x) {
  const outcomes = x.outcomes || (x.errors ? { error: x.errors, ok: Math.max(0, (x.calls || 0) - x.errors) } : { ok: x.calls || 0 });
  return {
    ...x,
    sql: x.sql || x.example || x.fingerprint,
    outcomes,
    first_seen_ms: x.first_seen_ms ?? null,
    last_seen_ms: x.last_seen_ms ?? x.last_ms ?? null,
    rows_per_call: x.rows_per_call ?? (x.calls ? (x.rows || 0) / x.calls : null),
    prev: x.prev || null,
    keys: x.keys || null,
  };
}

/** What changed for a statement against the window before: {calls, p99, verdict}. */
export function change(x) {
  const p = x.prev;
  if (!p || !p.calls) return null;
  const callsX = x.calls / Math.max(1, p.calls), p99X = p.p99_ms ? x.p99_ms / p.p99_ms : null;
  let verdict = null;
  if (p99X != null && p99X >= 2 && x.p99_ms > 20) verdict = callsX < 1.5 ? "slower, calls flat: likely a victim" : `slower with ${callsX.toFixed(1)}× the calls`;
  else if (callsX >= 3 && x.calls > 20) verdict = `${callsX.toFixed(1)}× the calls`;
  return { callsX, p99X, verdict, dTotal: (x.total_ms || 0) - (p.total_ms || 0) };
}

/**
 * Of the statements that took more time than in the window before: the
 * likely causes (new, or their calls grew by half or more) and the likely
 * victims (slower with their calls flat), each by the time they added, so
 * that a view names a cause before a victim. -> {causes, victims, cause}
 * of {x, c} (c: change(x), or {isNew, dTotal} for a new statement).
 */
export function causesAndVictims(xs, windowMs) {
  const byD = (a, b) => b.c.dTotal - a.c.dTotal;
  const grown = xs.map((x) => ({ x, c: change(x) })).filter((y) => y.c && y.c.dTotal > 0 && (y.c.p99X >= 1.5 || y.c.callsX >= 1.5));
  const news = xs.filter((x) => isNewStatement(x, windowMs)).map((x) => ({ x, c: { isNew: true, dTotal: x.total_ms || 0 } }));
  const causes = [...news, ...grown.filter((y) => y.c.callsX >= 1.5)].sort(byD);
  const victims = grown.filter((y) => y.c.callsX < 1.5).sort(byD);
  return { causes, victims, cause: causes[0] || null };
}

/** Why a statement needs a look, if it does: {tag, tone, rank} (higher first). */
export function attention(x, windowMs) {
  const sp = split(x.outcomes, x.calls);
  const ratio = sp.calls ? (sp.server + sp.client) / sp.calls : 0;
  if (sp.server && sp.server / Math.max(1, sp.calls) > 0.01) return { tag: "fails", tone: "bad", rank: 4 + ratio };
  if (sp.client && sp.client / Math.max(1, sp.calls) > 0.05) return { tag: "client errors", tone: "warn", rank: 3 + ratio };
  if (isNewStatement(x, windowMs)) return { tag: "new", tone: "info", rank: 2.5 };
  const c = change(x);
  if (c?.p99X >= 2 && x.p99_ms > 20) return { tag: `slower ×${c.p99X.toFixed(1)}`, tone: "warn", rank: 2 + Math.min(0.9, c.p99X / 100) };
  return null;
}

/**
 * One statement's page data, from `/v1/queries/stats/:fp`, or built from
 * the statements and the log store on a server without it.
 */
export async function statementDetail(fp, range = timeRange()) {
  try {
    // The statement, and from the window's list what of the range the server
    // holds and the statement's share of all the query time (its own answer
    // counts its share against itself).
    const [j, w] = await Promise.all([api(`/v1/queries/stats/${encodeURIComponent(fp)}?${rangeQuery(range)}`), api(`/v1/queries/stats?${rangeQuery(range)}&limit=1000`).catch(() => null)]);
    const statement = normStatement(j.statement || {});
    const listed = w?.statements?.find((x) => x.fingerprint === statement.fingerprint || x.id === fp || x.fingerprint === fp);
    if (listed?.share != null) statement.share = listed.share;
    else if (w?.statements) { const all = w.statements.reduce((a, x) => a + (x.total_ms || 0), 0); if (all) statement.share = (statement.total_ms || 0) / all; }
    return { ...j, statement, source: "stats", windowed: true, ...(w && !Array.isArray(w) ? held(w, range) : {}) };
  } catch (e) {
    if (e.status !== 404 && e.status !== 405) throw e;
  }
  // An older server: its statements since it started, and the log store's queries.
  const out = { statement: null, trend: [], by_key: [], by_shard: [], slowest: [], errors: [], source: "fallback" };
  try { const st = await statements(range); out.statement = st.statements.find((x) => x.fingerprint === fp) || null; out.windowed = st.windowed; out.since_ms = st.since_ms; } catch { /* left out */ }
  if (await hasLogStore()) {
    const lit = (v) => `'${String(v).replace(/'/g, "''")}'`;
    const w = `fingerprint = ${lit(fp)} AND shard = false AND ts >= ${lit(new Date(range.from).toISOString())}${range.to ? ` AND ts < ${lit(new Date(range.to).toISOString())}` : ""}`;
    const cols = await queryColumns();
    const extra = [cols.has("key") ? "key" : cols.has("key_name") ? "key_name AS key" : null, ...["addr", "app", "params", "client", "deadline_ms"].filter((c) => cols.has(c))].filter(Boolean);
    const [tr, sl, er, fm] = await Promise.all([
      ask(`SELECT minute, COUNT(*), AVG(ms), MAX(ms) FROM queries WHERE ${w} GROUP BY minute LIMIT 20000`).catch(() => null),
      ask(`SELECT ts, ms, outcome, request_id, sql, error, detail, id, node${extra.map((c) => ", " + c).join("")} FROM queries WHERE ${w} ORDER BY ms DESC LIMIT 12`).catch(() => null),
      ask(`SELECT error, COUNT(*), MAX(ts) FROM queries WHERE ${w} AND outcome != 'ok' GROUP BY error ORDER BY 2 DESC LIMIT 10`).catch(() => null),
      ask(`SELECT minute, COUNT(*) FROM queries WHERE ${w} AND outcome != 'ok' GROUP BY minute LIMIT 20000`).catch(() => null),
    ]);
    const failedAt = new Map((fm?.rows || []).map((r) => [r[0], r[1]]));
    if (tr) out.trend = tr.rows.filter((r) => r[0] != null).map((r) => ({ t: r[0] * 60e3, calls: r[1], errors: fm ? failedAt.get(r[0]) || 0 : null, mean_ms: r[2], p99_ms: r[3], max: true })).sort((a, b) => a.t - b.t);
    if (sl) out.slowest = objects(sl).map((r) => { const d = parseJson(r.detail) || {}; return { ...d, ...r, ts_ms: msOf(r.ts), params: r.params ?? d.params, key: r.key ?? d.key, addr: r.addr ?? d.addr, app: r.app ?? d.app }; });
    if (er) out.errors = er.rows.filter((r) => r[0]).map((r) => ({ message: r[0], count: r[1], last_ms: msOf(r[2]) }));
    out.trendNote = "per minute from the log store: the mean and the slowest call (no p99: the store keeps a sample of the fast calls)";
  }
  return out;
}

// ---------------------------------------------------------------- the log store

export const ask = (sql, scope) => api("/v1/logs/query", { body: scope ? { sql, scope } : { sql } });
export const objects = (j) => (j.rows || []).map((r) => Object.fromEntries(j.columns.map((c, i) => [c, r[i]])));
export const msOf = (ts) => typeof ts === "string" ? Date.parse(ts) : (ts || 0) / 1000;
export function parseJson(t) { if (!t) return null; if (typeof t === "object") return t; try { return JSON.parse(t); } catch { return null; } }

let storeInfo = null, storeAt = 0;
/** The log store's status and tables (cached a minute), or null without one. */
export async function logStore() {
  if (storeInfo && Date.now() - storeAt < 60e3) return storeInfo.value;
  try { const j = await api("/v1/logs/store"); storeInfo = { value: j }; }
  catch { storeInfo = { value: null }; }
  storeAt = Date.now();
  return storeInfo.value;
}
export async function hasLogStore() {
  const j = await logStore();
  return !!(j && (j.store || (j.nodes || []).some((n) => n.store)));
}
/** The columns of the store's `queries` table. */
export async function queryColumns() {
  const j = await logStore();
  const sc = (j?.schemas || []).find((x) => x.table === "queries");
  const fields = sc?.short?.fields || sc?.short?.columns || [];
  const names = new Set(fields.map((f) => f.name || f));
  if (!names.size && sc?.sql) for (const m of sc.sql.matchAll(/[(,]\s*([a-z_]+)\s+[A-Z]/g)) names.add(m[1]);
  return names;
}

// ---------------------------------------------------------------- the trace, summed up

const RPC = (x) => x.shard != null || x.replica;

/**
 * A trace's fan-out as rounds: each RPC name's calls that overlap in time
 * (a TermStats round, a Query round, a Fetch round), with their straggler.
 */
export function rounds(spans) {
  const out = [];
  for (const x of (spans || []).filter(RPC).slice().sort((a, b) => a.start_us - b.start_us)) {
    let r = out.find((o) => o.name === x.name && x.start_us <= o.end_us);
    if (!r) { r = { name: x.name, start_us: x.start_us, end_us: x.start_us + x.dur_us, calls: [] }; out.push(r); }
    r.calls.push(x);
    r.end_us = Math.max(r.end_us, x.start_us + x.dur_us);
  }
  for (const r of out) {
    const d = r.calls.map((c) => c.dur_us).sort((a, b) => a - b);
    r.median_us = d[Math.floor((d.length - 1) / 2)];
    r.slow = r.calls.reduce((a, c) => (c.start_us + c.dur_us > a.start_us + a.dur_us ? c : a), r.calls[0]);
    const rest = r.calls.filter((c) => c !== r.slow);
    r.restEnd_us = rest.length ? Math.max(...rest.map((c) => c.start_us + c.dur_us)) : r.start_us;
    r.shards = new Set(r.calls.map((c) => c.shard)).size;
    r.straggler = r.calls.length > 1 && r.slow.dur_us > 2 * r.median_us && r.slow.dur_us - r.median_us > 1000;
    r.failed = r.calls.filter((c) => c.outcome !== "ok" && c.outcome !== "outrun").length;
    r.again = r.calls.filter((c) => c.again).length;
  }
  return out;
}

/**
 * The trace's summary (mock 04): what the answer waited on, the critical
 * path a row per step, every shard's call on a log scale, hedging, and
 * the whole waterfall folded behind a button.
 */
export function traceSummary(q, { onNav } = {}) {
  const spans = q.trace || [];
  if (!spans.length) return h("div", { class: "small muted" }, "No trace was kept for this query.");
  const total_us = Math.max((q.ms || 0) * 1000, ...spans.map((x) => x.start_us + x.dur_us), 1);
  const rs = rounds(spans);
  const main = rs.slice().sort((a, b) => b.calls.length - a.calls.length || (b.end_us - b.start_us) - (a.end_us - a.start_us))[0];
  const worst = rs.reduce((a, r) => (!a || r.slow.dur_us > a.slow.dur_us ? r : a), null);
  const exec = spans.find((x) => x.name === "execute");
  const lastRpc = rs.length ? Math.max(...rs.map((r) => r.end_us)) : 0;
  const parts = [];
  const go = (href) => (e) => { onNav?.(); if (href) location.hash = href; e?.preventDefault?.(); };

  // The sentence: what the answer waited on.
  if (worst) {
    const name = copyName(worst.slow.replica);
    const story = [];
    if (worst.straggler) {
      story.push("The answer waited on ", h("b", {}, `shard ${worst.slow.shard}`), ", copy ", h("b", { class: "mono", title: copyTitle(worst.slow.replica) }, name), ": ",
        h("b", { class: "wl-bad" }, fmt.ms(worst.slow.dur_us / 1000)), `, ${(worst.slow.dur_us / Math.max(1, worst.median_us)).toFixed(0)}× the median ${RPC_WORDS[worst.name]?.[0] || worst.name} call (${fmt.ms(worst.median_us / 1000)}).`,
        worst.calls.length > 1 ? ` The other ${fmt.plural(worst.calls.length - 1, "call")} had answered by ${fmt.ms((worst.restEnd_us - worst.start_us) / 1000)}.` : "");
    } else {
      story.push(`${fmt.plural(new Set(rs.flatMap((r) => r.calls.map((c) => c.shard))).size, "shard")} asked in ${fmt.plural(rs.length, "round")}; no shard held it up: the slowest call was shard ${worst.slow.shard} (`, h("span", { class: "mono", title: copyTitle(worst.slow.replica) }, name), `) at ${fmt.ms(worst.slow.dur_us / 1000)}, the median ${fmt.ms(worst.median_us / 1000)}.`);
    }
    const coordUs = exec ? exec.dur_us : Math.max(0, total_us - lastRpc);
    if (coordUs > 0.25 * total_us && coordUs > 2000) story.push(" ", h("b", {}, `${fmt.ms(coordUs / 1000)} on the coordinator`), exec ? " (its execute step: a join, a sort or a merge it runs itself)" : " after the last shard answered (merging, joining or sorting there)", ".");
    parts.push(h("div", { class: "wl-story" }, story));
    if (worst.straggler || worst.slow.dur_us > 50e3) {
      const url = worst.slow.replica, t = q.ts_ms;
      parts.push(h("div", { class: "row", style: { gap: "6px" } },
        url ? h("a", { class: "btn sm", href: copyLink(url), onclick: go(copyLink(url)) }, `Open ${name}`) : null,
        url ? h("a", { class: "btn sm", href: link("logs", { node: name, from: t ? t - 60e3 : null, to: t ? t + 60e3 : null }), onclick: go(link("logs", { node: name, from: t ? t - 60e3 : null, to: t ? t + 60e3 : null })) }, `${name} logs around ${t ? fmt.time(t) : "then"}`) : null,
        url ? h("a", { class: "btn sm", href: nodeLink(url), onclick: go(nodeLink(url)) }, icon("flame"), `CPU profile of ${name}`) : null,
        h("a", { class: "btn sm", href: shardLink(worst.slow.shard), onclick: go(shardLink(worst.slow.shard)) }, `Shard ${worst.slow.shard}: compare its copies`)));
    }
  } else {
    parts.push(h("div", { class: "wl-story" }, exec ? `It ran on this server alone: ${fmt.ms(exec.dur_us / 1000)} executing.` : "It ran on this server alone: no shard was asked."));
  }

  // The critical path: a row per step on one scale.
  const rows = [];
  const X = (us) => (us / total_us * 100);
  const bar = (start, dur, cls, label, title) => h("div", { class: "lane" }, h("div", { class: "seg " + cls, style: { left: X(start) + "%", width: Math.max(0.4, X(dur)) + "%" }, title }, h("span", { class: "tiny" }, label)));
  for (const x of spans.filter((x) => !RPC(x))) {
    if (x.name === "execute") continue;
    rows.push(h("div", { class: "nm" }, h("b", {}, x.name)), bar(x.start_us, x.dur_us, x.name === "admission" ? "k-admission" : x.name === "plan" ? "k-plan" : x.name.startsWith("wait") ? "k-wait" : "k-rpc", fmt.ms(x.dur_us / 1000), `${x.name}: ${fmt.ms(x.dur_us / 1000)} from ${fmt.ms(x.start_us / 1000)}`));
  }
  for (const r of rs) {
    const n = r.calls.length;
    const rest = r.straggler ? r.calls.filter((c) => c !== r.slow) : r.calls;
    const restEnd = r.straggler ? r.restEnd_us : r.end_us;
    rows.push(h("div", { class: "nm" }, rpcName(r.name), h("span", { class: "badge" }, fmt.plural(rest.length, "shard")), r.failed ? h("span", { class: "pill bad", style: { padding: "0 6px" } }, `${r.failed} failed`) : null, r.again ? h("span", { class: "badge", title: "hedges or retries: calls asked again of another copy" }, `${r.again} again`) : null),
      bar(r.start_us, restEnd - r.start_us, "k-rpc" + (r.failed ? " bad" : ""), n > 1 ? `median ${fmt.ms(r.median_us / 1000)} · all done by ${fmt.ms((restEnd - r.start_us) / 1000)}` : fmt.ms(r.calls[0].dur_us / 1000), `${n} calls from ${fmt.ms(r.start_us / 1000)}`));
    if (r.straggler) {
      const c = r.slow, nm = copyName(c.replica);
      rows.push(h("div", { class: "nm" }, rpcName(r.name), h("span", { class: "badge" }, `shard ${c.shard}`), h("span", { class: "badge mono wl-badbadge", title: copyTitle(c.replica) }, nm)),
        bar(c.start_us, c.dur_us, "k-rpc bad", fmt.ms(c.dur_us / 1000), `${nm}: ${fmt.ms(c.dur_us / 1000)} from ${fmt.ms(c.start_us / 1000)}`));
    }
  }
  if (exec) rows.push(h("div", { class: "nm" }, h("b", {}, "execute"), h("span", { class: "badge" }, "on the coordinator")), bar(exec.start_us, exec.dur_us, "k-execute", fmt.ms(exec.dur_us / 1000), "the coordinator's own work"));
  else if (rs.length && total_us - lastRpc > 0) rows.push(h("div", { class: "nm" }, h("b", {}, "merge, answer"), h("span", { class: "badge" }, "on the coordinator")), bar(lastRpc, total_us - lastRpc, "k-execute", fmt.ms((total_us - lastRpc) / 1000), "after the last shard answered"));
  parts.push(h("div", { class: "small", style: { fontWeight: 600 } }, "Critical path"),
    h("div", { class: "wf" }, rows),
    h("div", { class: "wf" }, h("div"), h("div", { class: "scale" }, [0, 0.25, 0.5, 0.75, 1].map((f) => h("span", {}, fmt.ms(total_us * f / 1000))))));

  // Every call of the widest round, on a log scale.
  if (main && main.calls.length > 2) parts.push(h("div", { class: "small", style: { fontWeight: 600, marginTop: "6px" } }, `Every shard's ${main.name} call `, h("span", { class: "muted", style: { fontWeight: 400 } }, `${fmt.plural(main.calls.length, "call")} · log scale`)), callStrip(main));

  // Hedging: what asking another copy would have done.
  const hedge = state.cluster?.hedge_ms;
  if (worst?.straggler) {
    const after = Math.max(1, Math.ceil(worst.median_us * 3 / 1000));
    parts.push(h("div", { class: "notice" }, hedge ? `The coordinator asks another copy after ${fmt.ms(hedge)} (--hedge-ms). ` : "Hedging is off on this coordinator. ",
      `Asking another copy of shard ${worst.slow.shard} after ${fmt.ms(after)} would have answered this query in about ${fmt.ms((total_us - worst.slow.dur_us + Math.min(worst.slow.dur_us, (after * 1000) + worst.median_us)) / 1000)} (an estimate). `,
      h("a", { href: link("querypath", { sql: q.sql, p: paramsObj(q.params) ? JSON.stringify(paramsObj(q.params)) : null }), onclick: go(null) }, "Query path ›")));
  }

  // The whole waterfall, folded.
  const all = h("div");
  const more = h("button", { class: "btn sm", onclick: () => { if (all.childElementCount) { clear(all); more.lastChild.textContent = `Show all ${spans.length} spans ▸`; } else { clear(all, waterfall(spans, { byEnd: true })); more.lastChild.textContent = "Hide the spans ▴"; } } }, h("span", {}, `Show all ${spans.length} spans ▸`));
  if (spans.length > 1) parts.push(h("div", { class: "row" }, more, h("span", { class: "small muted" }, "sorted by end, the latest first")), all);
  return h("div", { class: "stack wl-trace" }, parts);
}

function callStrip(r) {
  const W = 760, H = 46, L = 4;
  const ds = r.calls.map((c) => c.dur_us / 1000);
  const lo = Math.max(0.05, Math.min(...ds) / 1.3), hi = Math.max(...ds) * 1.3;
  const X = (v) => L + (Math.log10(Math.max(v, lo)) - Math.log10(lo)) / Math.max(1e-9, Math.log10(hi) - Math.log10(lo)) * (W - 2 * L);
  const svgEl = s("svg", { viewBox: `0 0 ${W} ${H + 14}`, style: { width: "100%", height: H + 14 + "px" }, role: "img", "aria-label": `each ${r.name} call` });
  for (let e = Math.ceil(Math.log10(lo)); e <= Math.floor(Math.log10(hi)); e++) svgEl.append(s("line", { x1: X(10 ** e), x2: X(10 ** e), y1: 0, y2: H, style: { stroke: "var(--grid)" } }), s("text", { x: X(10 ** e), y: H + 11, "text-anchor": "middle", style: { fill: "var(--muted)", fontSize: "10px" } }, fmt.ms(10 ** e)));
  r.calls.forEach((c, i) => {
    const v = c.dur_us / 1000, slow = c === r.slow && r.straggler;
    const dot = s("circle", { cx: X(v), cy: slow ? H / 2 : 8 + ((i * 7919) % 30), r: slow ? 5 : 3, style: { fill: slow ? "var(--crit)" : c.outcome !== "ok" && c.outcome !== "outrun" ? "var(--s8)" : "var(--s1)", opacity: slow ? 1 : 0.7 } });
    dot.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `shard ${c.shard} · ${copyName(c.replica)}`), h("div", { class: "r" }, h("b", {}, fmt.ms(v)), h("span", { class: "n" }, c.outcome + (c.again ? " · asked again" : "")))]));
    dot.addEventListener("pointerleave", hideTip);
    svgEl.append(dot);
    if (slow) svgEl.append(s("text", { x: Math.min(W - 4, X(v)), y: H / 2 - 9, "text-anchor": "end", style: { fill: "var(--crit-ink)", fontSize: "10.5px" } }, `shard ${c.shard} · ${copyName(c.replica)}`));
  });
  return svgEl;
}

/** A trace as a waterfall: a lane per span, on one scale; copies by short name. */
/** QRY-12: a call to a shard in words, with the gRPC method in its title. */
const RPC_WORDS = { TermStats: ["term counts", "each shard's term counts, so that BM25 scores alike on every shard"], Query: ["shard's part", "the shard runs its part of the query"], Search: ["shard search", "the shard searches its rows"], Fetch: ["fetch rows", "the rows of the hits, from the shards that hold them"], Lookup: ["rows by key", "rows read by their keys"], Write: ["write", "the shard writes its rows"] };
/** A shard call's name as a row head: in words when known. */
const rpcName = (n) => h("b", { title: RPC_WORDS[n] ? `${RPC_WORDS[n][1]} (gRPC ${n})` : null }, RPC_WORDS[n]?.[0] || n);
export function waterfall(spans, { byEnd = false } = {}) {
  if (!spans?.length) return h("div", { class: "small muted" }, "No trace was kept for this query.");
  const end = Math.max(...spans.map((x) => x.start_us + x.dur_us), 1);
  const kind = (n) => n === "admission" ? "k-admission" : n === "execute" ? "k-execute" : n === "plan" ? "k-plan" : n.startsWith("wait") ? "k-wait" : "k-rpc";
  const rows = spans.slice().sort(byEnd ? (a, b) => (b.start_us + b.dur_us) - (a.start_us + a.dur_us) : (a, b) => a.start_us - b.start_us);
  return h("div", {},
    h("div", { class: "wf" }, rows.map((x) => [
      h("div", { class: "nm", title: x.replica ? copyTitle(x.replica) : "" }, h("b", { title: RPC_WORDS[x.name] ? `${RPC_WORDS[x.name][1]} (gRPC ${x.name})` : null }, RPC_WORDS[x.name]?.[0] || x.name), x.shard != null ? h("span", { class: "badge" }, "shard " + x.shard) : null, x.replica ? h("span", { class: "badge mono" }, copyName(x.replica)) : null, x.again ? h("span", { class: "badge", title: "a hedge or a retry" }, "again") : null, x.outcome !== "ok" ? h("span", { class: "pill " + (x.outcome === "outrun" ? "info" : "bad"), style: { padding: "0 6px" }, title: x.outcome === "outrun" ? "another copy answered first" : null }, x.outcome) : null),
      h("div", { class: "lane" }, h("div", { class: `seg ${kind(x.name)}${x.outcome !== "ok" && x.outcome !== "outrun" ? " bad" : ""}${x.again ? " again" : ""}`, style: { left: (x.start_us / end * 100) + "%", width: Math.max(0.4, x.dur_us / end * 100) + "%" }, title: `${RPC_WORDS[x.name]?.[0] || x.name}: ${fmt.ms(x.dur_us / 1000)} from ${fmt.ms(x.start_us / 1000)}${x.replica ? " on " + copyName(x.replica) : ""}` },
        h("span", { class: "tiny" }, fmt.ms(x.dur_us / 1000)))),
    ]),
    h("div", { class: "scale" }, [0, 0.25, 0.5, 0.75, 1].map((f) => h("span", {}, fmt.ms(end * f / 1000))))));
}

// ---------------------------------------------------------------- the query drawer

/** What a query's deadline was and how it ended, in words. */
function deadlineText(q) {
  const lim = q.deadline_ms ?? null, server = state.system?.limits?.timeout_ms;
  if (lim != null && server && lim < server) return `${fmt.ms(lim)}, the client's own (the server's is ${fmt.ms(server)})`;
  if (lim != null) return fmt.ms(lim);
  return server ? `${fmt.ms(server)}, the server's (the query did not send its own)` : "–";
}

/**
 * The details of one query that ended: who sent it and with what, how it
 * ended against its deadline, the trace summed up, its profile, the
 * rest of its request and its log lines, and the links onward.
 */
export function queryDrawer(q) {
  const kept = q.steps?.length > 0, fetch = !kept && q.id != null && q.steps_n && !q.node;
  const logs = h("div"), same = h("div");
  const prof = h("div", { class: "small muted" }, "Reading its profile…");
  const fp = q.fingerprint || roughFp(q.sql);
  const params = paramsObj(q.params);
  const o = outcomeOf(q.outcome);
  let d = null;
  const close = () => d?.close();
  const kv = [
    ["who", whoChips(q)],
    params ? ["params", h("span", { class: "mono small" }, JSON.stringify(params))] : q.params === undefined ? null : ["params", h("span", { class: "muted" }, "none")],
    ["deadline", deadlineText(q)],
    ["outcome", h("span", { class: "row", style: { gap: "6px", flexWrap: "wrap" } }, outcomePill(q.outcome), h("span", { class: "small dim" }, o.says))],
    q.node ? ["server", h("span", { class: "mono" }, q.node)] : null,
    ["statement", h("a", { href: statementLink(fp), onclick: close, class: "wl-link mono small", title: fp }, `${shortFp(fp)} › its page`)],
    ["request id", q.request_id ? h("span", { class: "row nw" }, h("span", { class: "mono" }, q.request_id), h("button", { class: "btn ghost icon sm", title: "Copy", onclick: () => copy(q.request_id) }, icon("copy")), h("a", { class: "btn ghost sm", href: link("logs", { request: q.request_id }), onclick: close }, "Its logs", icon("right"))) : "–"],
  ].filter(Boolean);
  d = drawer({
    title: h("span", { class: "row nw" }, outcomePill(q.outcome), `${fmt.ms(q.ms)} · ${q.kind || "query"}`),
    sub: `${fmt.datetime(q.ts_ms)} · ${fmt.plural(q.rows ?? 0, "row")}${q.shard ? " · a shard's part of a coordinator's query" : ""}`, wide: true,
    body: [
      h("div", { class: "row between" }, h("b", {}, "SQL"), h("div", { class: "row" },
        h("button", { class: "btn sm", onclick: () => copy(q.sql, "SQL copied") }, icon("copy"), "Copy"),
        h("a", { class: "btn sm", href: profileLink(q.sql, params, q.fingerprint || null), title: "Open it in the Profiler with its parameters: it asks before it runs", onclick: close }, icon("flame"), "Profile it again"),
        h("a", { class: "btn sm", href: workbenchLink(q.sql, params), title: "Open it in the SQL workbench, with its parameters", onclick: close }, icon("play"), "Open in the workbench"))),
      sqlBlock(q.sql),
      q.error ? h("div", { class: o.server ? "error" : "notice" }, q.error) : null,
      h("dl", { class: "kv wl-kv" }, kv.flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
      card({ title: "Trace", sub: "where the time went", body: traceSummary(q, { onNav: close }) }),
      kept || fetch ? card({ title: "Profile", sub: q.cpu_ms != null ? `every step it took, with its CPU time (${fmt.ms(q.cpu_ms)})` : "every step it took", body: prof }) : null,
      q.request_id ? card({ title: "Everything this request ran", body: same }) : null,
      q.request_id ? card({ title: "Its log lines", body: logs }) : null,
    ],
  });
  if (kept) clear(prof, profileView({ ...q, took_ms: q.ms }, { compact: true }));
  else if (fetch) api("/v1/queries/finished/" + q.id).then((f) => clear(prof, profileView({ ...f, took_ms: f.ms }, { compact: true }))).catch((e) => clear(prof, e.status === 404 ? "Its profile is no longer kept: the server keeps the profiles of its latest and slowest queries. Profile it again to see one." : e.message));
  if (q.request_id) {
    api("/v1/queries/request/" + encodeURIComponent(q.request_id)).then((j) => clear(same, j.queries.length > 1 ? j.queries.map((x) => h("div", { class: "row nw small", style: { padding: "3px 0" } }, outcomePill(x.outcome), h("span", { class: "mono ellipsis grow" }, x.sql), h("b", { class: "num" }, fmt.ms(x.ms)))) : h("div", { class: "small muted" }, "Just this query."))).catch((e) => clear(same, h("div", { class: "small muted" }, e.message)));
    api("/v1/logs?limit=50&request=" + encodeURIComponent(q.request_id)).then((j) => clear(logs, j.lines.length ? h("div", { class: "logs", style: { height: "auto", maxHeight: "240px", minHeight: 0 } }, j.lines.map((l) => h("div", { class: "ln" }, h("span", { class: "muted" }, fmt.time(l.ts_ms)), h("span", { class: "lv " + l.level }, l.level), h("span", { class: "tg" }, l.target), h("span", { class: "msg" }, l.message)))) : h("div", { class: "small muted" }, "No lines for it on this server: requests log at debug. ", h("a", { href: link("logs", { request: q.request_id }), onclick: close }, "Search every server's logs ›")))).catch((e) => clear(logs, h("div", { class: "small muted" }, e.message)));
  }
  return d;
}

/** A running query's drawer: its trace so far, who sent it, Cancel. */
export function liveDrawer(q, { onCancel } = {}) {
  const params = paramsObj(q.params);
  let d = null;
  const close = () => d?.close();
  d = drawer({
    title: `Query ${q.id}`, sub: `running for ${fmt.ms(q.elapsed_ms)}${q.node ? " on " + q.node : ""}`, wide: true,
    body: [
      sqlBlock(q.sql),
      h("dl", { class: "kv wl-kv" }, [
        ["who", whoChips(q)],
        ["params", params ? h("span", { class: "mono small" }, JSON.stringify(params)) : h("span", { class: "muted" }, "none")],
        ["deadline", deadlineText(q)],
        ["request id", q.request_id ? h("span", { class: "row nw" }, h("span", { class: "mono" }, q.request_id), h("a", { class: "btn ghost sm", href: link("logs", { request: q.request_id }), onclick: close }, "Its logs", icon("right"))) : "–"],
        ["statement", h("a", { href: statementLink(q.fingerprint || roughFp(q.sql)), onclick: close, class: "wl-link mono small" }, `${shortFp(q.fingerprint || roughFp(q.sql))} › its page`)],
      ].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
      card({ title: "Trace so far", sub: "the calls that answered; one still out is not drawn", body: traceSummary({ ...q, ms: q.elapsed_ms }, { onNav: close }) }),
    ],
    foot: onCancel && can("admin", "DELETE /v1/queries/:id") ? [h("button", { class: "btn", onclick: close }, "Close"), h("button", { class: "btn danger", onclick: () => { close(); onCancel(q); } }, icon("stop"), "Cancel this query…")] : null,
  });
  return d;
}

// ---------------------------------------------------------------- cancel and contain

const MINUTES = [[15, "15 min"], [60, "1 h"], [240, "4 h"], [1440, "24 h"]];

/**
 * Asks before cancelling running queries (mock 03b): which, who sent
 * them, what their clients get, a reason for the audit log, and whether
 * to refuse new runs for a while. Focus starts on "Keep them running".
 * Resolves {reason, refuse_min} or false.
 */
export function cancelDialog({ sql, queries, key, fingerprint = null, refusable = true }) {
  return new Promise((resolve) => {
    const n = queries.length;
    const keys = countBy(queries, (q) => q.key || null);
    const vias = countBy(queries, (q) => q.node || null);
    const el = queries.map((q) => q.elapsed_ms).sort((a, b) => a - b);
    const mem = queries.reduce((a, q) => a + (q.memory_bytes || 0), 0);
    const reason = h("input", { class: "input", placeholder: "e.g. INC-2291: runaway self-join from search-batch", style: { width: "100%" }, "aria-label": "Reason, for the audit log" });
    const oneKey = keys.length === 1 && keys[0][0] ? keys[0][0] : key || null;
    const refuse = h("input", { type: "checkbox" });
    let minutes = 15;
    const minSel = h("select", { class: "select sm", style: { minWidth: "84px" }, "aria-label": "for how long", onchange: () => { minutes = +minSel.value; } }, MINUTES.map(([v, l]) => h("option", { value: v }, l)));
    const ok = h("button", { class: "btn danger solid", disabled: true }, n === 1 ? "Cancel the query" : `Cancel ${fmt.n(n)} queries`);
    const keep = h("button", { class: "btn" }, n === 1 ? "Keep it running" : "Keep them running");
    reason.oninput = () => { ok.disabled = !reason.value.trim(); };
    let done = false;
    // NEW-workload-8: queries end and start while the dialog is open; recount them every 2 s.
    const now = h("dd", { class: "small" }, "checking…");
    const recount = async () => {
      try {
        const j = await api("/v1/queries");
        if (done) return;
        const run = (j.queries || []).filter((q) => !q.cancelled && (fingerprint ? q.fingerprint === fingerprint && (!key || q.key === key) : queries.some((x) => x.id === q.id)));
        const ids = new Set(queries.map((q) => q.id)), ended = queries.filter((q) => !run.some((r) => r.id === q.id)).length, started = run.filter((r) => !ids.has(r.id)).length;
        now.textContent = !run.length ? `none running: ${n === 1 ? "it has" : "they have"} ended` : `${fmt.n(run.length)} running${ended ? `; ${fmt.n(ended)} of the ${fmt.n(n)} ${ended === 1 ? "has" : "have"} ended` : ""}${started ? `; ${fmt.n(started)} started since` : ""}`;
        ok.textContent = !run.length ? "Nothing to cancel" : run.length === 1 ? "Cancel the query" : `Cancel ${fmt.n(run.length)} queries`;
        ok.dataset.none = run.length ? "" : "1";
        ok.disabled = !run.length || !reason.value.trim();
      } catch { now.textContent = "could not be counted again"; }
    };
    reason.oninput = () => { ok.disabled = !reason.value.trim() || ok.dataset.none === "1"; };
    const timer = setInterval(recount, 2000);
    const m = modal({
      title: n === 1 ? "Cancel this running query?" : `Cancel ${fmt.n(n)} running queries of one statement?`,
      body: [
        sqlBlock(sql),
        h("dl", { class: "kv wl-kv" },
          h("dt", {}, "Which"), h("dd", {}, `${fmt.plural(n, "query", "queries")}${vias.length && vias[0][0] ? ": " + vias.map(([v, c]) => `${c} on ${v}`).join(", ") : ""} · running ${fmt.ms(el[0])}${n > 1 ? ` to ${fmt.ms(el[el.length - 1])}` : ""}${mem ? ` · holding ${fmt.bytes(mem)} of query memory` : ""}`),
          h("dt", {}, "Who sent them"), h("dd", {}, keys.some(([k]) => k) ? keys.map(([k, c], i) => [i ? ", " : "", k ? ["API key ", h("b", { class: "mono" }, k)] : "no key", keys.length > 1 ? ` (${c})` : ""]) : h("span", { class: "muted" }, "unknown: this server does not record the key")),
          h("dt", {}, "Now"), now,
          h("dt", {}, "What they get"), h("dd", {}, "HTTP 499 ", h("span", { class: "mono" }, "cancelled by an operator"), ". A client that retries sends them again.")),
        !refusable ? null : h("label", { class: "row small wl-refuse" }, refuse, h("span", {}, oneKey ? ["Also refuse new runs of this statement from ", h("b", { class: "mono" }, oneKey), " for"] : "Also refuse new runs of this statement for"), minSel, h("span", { class: "muted" }, "(they get 429)")),
        h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "Reason, for the audit log"), reason),
      ],
      foot: [keep, ok],
      onClose: () => { clearInterval(timer); if (!done) resolve(false); },
    });
    recount();
    keep.onclick = () => { done = true; clearInterval(timer); m.close(); resolve(false); };
    ok.onclick = () => { if (!reason.value.trim()) return; done = true; clearInterval(timer); m.close(); resolve({ reason: reason.value.trim(), refuse_min: refuse.checked ? minutes : 0, key: oneKey }); };
    reason.onkeydown = (e) => { if (e.key === "Enter") e.preventDefault(); };
    setTimeout(() => keep.focus(), 0);
  });
}

function countBy(xs, f) { const m = new Map(); for (const x of xs) { const k = f(x); m.set(k, (m.get(k) || 0) + 1); } return [...m].sort((a, b) => b[1] - a[1]); }

/**
 * Cancels running queries: of one shape (and key) with one call on a
 * server that has bulk cancel, else one by one. -> number cancelled.
 */
export async function cancelQueries({ fingerprint, key, queries, reason }) {
  if (fingerprint && queries.length > 1) {
    try {
      const qs = new URLSearchParams({ fingerprint }); if (key) qs.set("key", key);
      const j = await api("/v1/queries?" + qs, { method: "DELETE", reason });
      return j.cancelled ?? queries.length;
    } catch (e) { if (e.status !== 404 && e.status !== 405 && e.status !== 400) throw e; }
  }
  let n = 0, gone = 0;
  for (const q of queries) {
    try { await api("/v1/queries/" + encodeURIComponent(q.id), { method: "DELETE", reason }); n++; } catch (e) { if (e.status !== 404) throw e; gone++; }
  }
  cancelQueries.gone = gone;
  return n;
}

/** Cancels after asking; then refuses the shape for a while when asked. */
export async function askCancel({ sql, fingerprint, key, queries, onDone }) {
  if (!can("admin", "DELETE /v1/queries/:id")) { toast(`Cancelling ${needs("admin", "DELETE /v1/queries/:id")}`, "bad"); return; }
  const a = await cancelDialog({ sql, queries, key, fingerprint, refusable: !!fingerprint && await canContain() });
  if (!a) return;
  // The rule first, so a client that retries at once is refused rather than started again.
  if (a.refuse_min && fingerprint) {
    try {
      await addContain({ match: { fingerprint, ...(a.key ? { key: a.key } : {}) }, action: "refuse", expires_s: a.refuse_min * 60, reason: a.reason });
      toast(`New runs are refused for ${a.refuse_min >= 60 ? fmt.plural(a.refuse_min / 60, "hour") : a.refuse_min + " min"}`, "ok");
    } catch (e) { toast(e.status === 404 ? "This server cannot refuse a statement: it is older than contain rules" : "Not refused: " + e.message, "bad", 7000); }
  }
  try {
    cancelQueries.gone = 0;
    const n = await cancelQueries({ fingerprint, key, queries, reason: a.reason });
    const gone = cancelQueries.gone;
    toast(n ? `${fmt.plural(n, "query", "queries")} cancelled: ${n === 1 ? "it answers" : "they answer"} 499${gone ? `. ${gone} had already ended` : ""}` : `Nothing to cancel: ${gone === 1 ? "it had" : "they had"} already ended`, n ? "ok" : "info");
  } catch (e) { toast(e.message, "bad"); }
  onDone?.();
}

export const addContain = (rule) => api("/v1/admin/contain", { body: rule, reason: rule.reason });

/** The contain rules in force, or null on a server without them. */
/** Whether the server has contain rules: null until asked, then true or false. */
let containable = null;
export async function containRules() {
  try { const j = await api("/v1/admin/contain"); containable = true; return Array.isArray(j) ? j : j.rules || []; }
  catch (e) { if (e.status === 404 || e.status === 405) { containable = false; return null; } throw e; }
}
/** Whether to offer Contain: an admin key, on a server not known to lack it. */
export const canContainNow = () => can("admin", "POST /v1/admin/contain") && containable !== false;
/** Whether this server can contain: asks once. */
export async function canContain() { if (containable == null) { try { await containRules(); } catch { /* unknown: offer it */ } } return containable !== false; }

/**
 * Limits or refuses a statement and/or a key for a while (scenario mock 03):
 * what it does to whom, how long, and a reason; audited and an event.
 */
export function containDialog({ fingerprint, sql, key, keys, onDone, action: act = "limit" } = {}) {
  if (!can("admin", "POST /v1/admin/contain")) { toast(`Containing ${needs("admin", "POST /v1/admin/contain")}`, "bad"); return; }
  if (containable === false) { toast("This server cannot contain a statement or key yet: contain rules are newer than it", "bad", 6000); return; }
  // R2-workload-2: from a statement and a key, the statement from that key (not everything the key sends),
  // with the action the button named.
  let action = act === "refuse" ? "refuse" : "limit", scope = fingerprint && key ? "both" : fingerprint ? "statement" : "key", minutes = 60, conc = 2;
  // Each key's share of the statement, from its calls when the caller gave none.
  const callsAll = (keys || []).reduce((a, x) => a + (x.calls || 0), 0);
  keys = (keys || []).map((x) => ({ ...x, share: x.share ?? (callsAll ? (x.calls || 0) / callsAll : null) }));
  const keyIn = h("input", { class: "input sm mono", value: key || "", placeholder: "key name", style: { width: "160px" }, "aria-label": "API key" });
  const concIn = h("input", { class: "input sm num", type: "number", min: 0, value: conc, style: { width: "64px" }, "aria-label": "at most running at once", oninput: () => { conc = Math.max(0, +concIn.value || 0); draw(); } });
  const reason = h("input", { class: "input", placeholder: "e.g. Sorting 5,000 rows a call slows search for everyone", style: { width: "100%" }, "aria-label": "Reason" });
  const say = h("div", { class: "notice" });
  const ok = h("button", { class: "btn primary", disabled: true }, "Apply");
  const cancel = h("button", { class: "btn" }, "Cancel");
  const actions = h("div", { class: "stack", style: { gap: "8px" } });
  const scopes = h("div");
  function draw() {
    clear(actions,
      radio("act", action === "limit", () => { action = "limit"; draw(); }, h("span", {}, h("b", {}, "Limit it to "), concIn, h("b", {}, " running at once"), h("div", { class: "small muted" }, "the rest wait for a turn, then answer 429"))),
      radio("act", action === "refuse", () => { action = "refuse"; draw(); }, h("span", {}, h("b", {}, "Refuse it"), h("div", { class: "small muted" }, "every new run answers 429 naming this rule and your reason; running ones finish"))));
    const k = keyIn.value.trim();
    clear(scopes, h("div", { class: "row", style: { gap: "10px" } },
      fingerprint ? seg([["statement", "this statement, every key"], ["both", "this statement from one key"], ["key", "everything from one key"]], scope, (v) => { scope = v; draw(); }, { class: "seg wl-segwrap" }) : h("span", { class: "small dim" }, "Everything from the key"),
      scope !== "statement" ? keyIn : null),
      keys?.length && scope !== "statement" ? h("div", { class: "row small", style: { gap: "6px", marginTop: "6px" } }, h("span", { class: "muted" }, "Its keys:"), keys.slice(0, 6).map((x) => h("button", { class: "chip", "aria-pressed": String(x.key === k), onclick: () => { keyIn.value = x.key; draw(); } }, x.share == null ? x.key : `${x.key} ${fmt.pct(x.share, 0)}`))) : null);
    const who = scope === "statement" ? "every client that sends this statement" : scope === "both" ? `key ${k || "…"} when it sends this statement; other keys keep running it` : `everything key ${k || "…"} sends`;
    clear(say, `${action === "limit" ? `At most ${conc} at once` : "Refused"} for ${MINUTES.find((x) => x[0] === minutes)?.[1]}: ${who}. It holds on every coordinator, ends by itself, is written to the audit log with your reason, and shows on the Timeline.`);
    ok.disabled = !reason.value.trim() || (scope !== "statement" && !k);
    ok.textContent = action === "limit" ? "Apply the limit" : "Refuse it";
  }
  keyIn.oninput = draw; reason.oninput = draw;
  const m = modal({
    title: fingerprint ? "Contain a statement" : "Contain a key",
    body: [
      sql ? sqlBlock(sql, { class: "code wl-clip" }) : null,
      actions, scopes,
      h("div", { class: "row", style: { gap: "8px" } }, h("span", { class: "small dim" }, "Ends after"), seg(MINUTES, minutes, (v) => { minutes = v; draw(); })),
      h("label", { class: "stack", style: { gap: "4px" } }, h("span", { class: "small dim" }, "Reason, for the audit log and the clients' 429"), reason),
      say,
    ],
    foot: [cancel, ok],
  });
  cancel.onclick = () => m.close();
  ok.onclick = async () => {
    const k = keyIn.value.trim();
    const match = scope === "statement" ? { fingerprint } : scope === "both" ? { fingerprint, key: k } : { key: k };
    try {
      await addContain({ match, action, ...(action === "limit" ? { concurrency: conc } : {}), expires_s: minutes * 60, reason: reason.value.trim() });
      m.close(); toast(action === "limit" ? "Limit applied" : "Refusing it now", "ok"); onDone?.();
    } catch (e) { toast(e.status === 404 || e.status === 405 ? "This server cannot contain a statement: it is older than contain rules" : e.message, "bad", 7000); }
  };
  draw();
  setTimeout(() => reason.focus(), 0);
}

function radio(name, on, pick, label) {
  const r = h("input", { type: "radio", name, checked: on, onchange: pick });
  return h("label", { class: "row nw wl-radio", style: { gap: "8px", alignItems: "flex-start" } }, r, label);
}

/** The contain rules as a card body: what each matches, does, until when, its hits; Remove. */
export function containList(rules, { onChange } = {}) {
  if (rules == null) return h("div", { class: "small muted" }, "This server has no contain rules yet: a newer one limits or refuses a statement or a key for a while.");
  if (!rules.length) return h("div", { class: "small muted" }, "No statement or key is limited or refused now.");
  return h("div", { class: "stack", style: { gap: "6px" } }, rules.map((r) => h("div", { class: "row nw wl-rule" },
    pill(r.action === "refuse" ? "warn" : "info", r.action === "refuse" ? "refused" : `at most ${r.concurrency ?? "?"} at once`),
    h("div", { class: "grow", style: { minWidth: 0 } },
      h("div", { class: "row nw", style: { gap: "6px" } }, r.match?.fingerprint ? h("a", { class: "mono small ellipsis wl-link", href: statementLink(r.match.fingerprint), title: r.match.fingerprint }, r.match.fingerprint) : h("span", { class: "small" }, "every statement"), r.match?.key ? [h("span", { class: "small muted" }, "from"), h("b", { class: "mono small" }, r.match.key)] : null),
      h("div", { class: "small muted" }, `${r.by ? "by " + r.by + " · " : ""}${r.reason ? "“" + r.reason + "” · " : ""}ends ${r.expires_ms ? fmt.ago(r.expires_ms).replace(/^in /, "in ") : "–"}${r.hits != null ? ` · ${fmt.n(r.hits)} matched` : ""}${r.refused != null ? `, ${fmt.n(r.refused)} refused` : ""}`)),
    can("admin", "DELETE /v1/admin/contain/:id") ? h("button", { class: "btn sm", onclick: async () => {
      // Lifting a rule lets its clients back in at once: ask, and keep why in the audit log.
      const ok = await core.confirm({ title: "Remove this rule?", body: `${r.match?.key ? `Key ${r.match.key}` : "Every client"} can run ${r.match?.fingerprint ? "this statement" : "everything"} again at once${r.action === "limit" ? ", with no limit" : ""}. The audit log keeps who removed it and why.`, ok: "Remove the rule", reason: true });
      if (!ok) return;
      try { await api("/v1/admin/contain/" + encodeURIComponent(r.id), { method: "DELETE", reason: ok.reason || "removed from the command center" }); toast("Rule removed", "ok"); onChange?.(); } catch (e) { toast(e.message, "bad"); }
    } }, "Remove") : null)));
}
