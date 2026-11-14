// What every view reads, fetched once for all of them: the series of
// points, the server's health, system, alerts and cluster, the fleet's
// health, and who the viewer is. Views listen on the bus.
//
// The time range is global and lives in the URL: `?range=15m|1h|6h|24h|7d`,
// or `?from=<ms>&to=<ms>` for a fixed window (a link with from/to opens
// paused). Up to an hour the series is the server's 2 s points, polled;
// longer or older windows read `/v1/timeseries?from=&to=` (per-minute points
// from the server's `load` table). While paused nothing redraws.

import { api, bus, every, store, provide, provideRoutes, roleOf, connection, fmt, span, can, isIdle, IDLE_SLOW } from "./core.js";

/** The ranges the picker offers: [key, minutes]. */
export const RANGES = [["15m", 15], ["1h", 60], ["6h", 360], ["24h", 1440], ["7d", 10080]];
const DEFAULT_RANGE = 60;
const MEMORY_MIN = 60; // the server keeps an hour of 2 s points in memory
const rangeKey = (min) => (RANGES.find(([, m]) => m === min) || [null])[0];
const rangeMin = (key) => (RANGES.find(([k]) => k === key) || [null, null])[1];
const validRange = (m) => (RANGES.some(([, x]) => x === m) ? m : DEFAULT_RANGE);

export const state = {
  range: validRange(store.get("range", DEFAULT_RANGE)), // minutes, when no fixed window
  from: null, to: null, // a fixed window (epoch ms), or null for the last `range` minutes
  zoom: null, // [t0, t1] picked with a brush
  paused: false, pausedAt: null,
  points: [],
  interval: 2000,
  /** Where the points come from: "memory" (2 s points) or "load" (per-minute); `partial` when the server lacks part of the window. */
  series: { source: "memory", interval_ms: 2000, from: null, to: null, partial: false, loaded_ms: null },
  health: null, system: null, alerts: null, cluster: null, whoami: null,
  /** GET /v1/kube, once a view asked (kubeInfo()): whether an operator manages the cluster. */
  kube: null,
  /** GET /v1/health/fleet, or the same shape worked out here from the cluster when the server lacks it (`derived: true`). */
  fleet: null,
  /** Why the key may not see the fleet's health (a key limited to tables), or null. */
  fleetRefused: null,
  online: true, error: null,
};

export const isCoordinator = () => state.health?.role === "coordinator" || state.system?.backend === "coordinator" || state.cluster?.role === "coordinator";

// ---------------------------------------------------------------- the time range
/**
 * The window views show: { from, to, live, label, range, fixed, zoomed }.
 * `live` is true while it follows now; `range` is "1h" etc. for a relative
 * one; `fixed` for a from/to window; `zoomed` when a brush picked it.
 */
export function timeRange() {
  if (state.zoom) return { from: state.zoom[0], to: state.zoom[1], live: false, label: span(state.zoom[0], state.zoom[1]), range: null, fixed: true, zoomed: true };
  if (state.from != null) return { from: state.from, to: state.to, live: false, label: span(state.from, state.to), range: null, fixed: true, zoomed: false };
  const to = state.paused && state.pausedAt ? state.pausedAt : Date.now();
  const key = rangeKey(state.range);
  return { from: to - state.range * 60e3, to, live: !state.paused, label: "last " + (key || state.range + "m").replace(/(\d)([a-z])/, "$1 $2"), range: key, fixed: false, zoomed: false };
}
/**
 * The shortest live range that covers `t` (epoch ms): {range: "15m"} etc.,
 * or a fixed window from `t` to now when it is older than the longest. A
 * link "since T" stays live: it does not pause the views it carries into.
 */
export function rangeSince(t) {
  const back = (Date.now() - t) / 60e3;
  const r = RANGES.find(([, m]) => m >= back);
  return r ? { range: r[0] } : { from: Math.round(t), to: Date.now() };
}
/** Last night, 18:00 to 08:00; before 08:00, yesterday 18:00 to now (the night still going). */
export function overnight() { const d = new Date(); d.setHours(8, 0, 0, 0); return d > Date.now() ? [d.getTime() - 14 * 3600e3, Date.now()] : [d.getTime() - 14 * 3600e3, d.getTime()]; }
/** Whether the window shown ended a while ago (a past window, not now). */
export const isPast = () => { const r = timeRange(); return r.fixed && r.to < Date.now() - 90e3; };
/** The range as URL params: {range: "6h"}, {from, to}, or {} for the default. Zoom counts as a window. */
export function rangeParams({ absolute = false } = {}) {
  const r = timeRange();
  if (r.fixed || absolute) return { from: Math.round(r.from), to: Math.round(r.to) };
  return state.range === DEFAULT_RANGE ? {} : { range: r.range };
}

/** The span of time the charts show. */
export function domain() {
  if (state.zoom) return state.zoom;
  if (state.from != null) return [state.from, state.to];
  const t1 = state.paused && state.pausedAt ? state.pausedAt : state.points.length ? Math.max(state.points[state.points.length - 1].t, Date.now() - 2 * state.interval) : Date.now();
  if (state.range > MEMORY_MIN) return [t1 - state.range * 60e3, t1];
  // While the series is younger than the range, it fills the width.
  const first = state.points.length ? state.points[0].t : t1;
  return [Math.max(t1 - state.range * 60e3, Math.min(first, t1 - 60e3)), t1];
}
export function visible() {
  const [a, b] = domain();
  return state.points.filter((p) => p.t >= a - state.interval && p.t <= b + state.interval);
}

/**
 * Picks the range: setRange(60) or setRange("6h") for the last N minutes
 * (live again), setRange({from, to}) for a fixed window (paused).
 */
export function setRange(r) {
  if (r && typeof r === "object") return setWindow(r.from, r.to);
  const min = typeof r === "string" ? rangeMin(r) ?? (parseInt(r, 10) || DEFAULT_RANGE) : r;
  const was = seriesKey();
  state.range = validRange(min); state.from = null; state.to = null; state.zoom = null;
  store.set("range", state.range);
  if (state.paused) { state.paused = false; state.pausedAt = null; provide({ paused: false }); bus.emit("paused"); }
  writeUrl();
  bus.emit("range");
  if (seriesKey() !== was) refreshSeries();
}
/** A fixed window, paused: setWindow(fromMs, toMs). */
export function setWindow(from, to) {
  from = Math.round(+from); to = Math.round(+to);
  if (!(from > 0 && to > from)) return;
  state.from = from; state.to = to; state.zoom = null;
  if (!state.paused) { state.paused = true; state.pausedAt = to; provide({ paused: true }); bus.emit("paused"); }
  writeUrl();
  bus.emit("range");
  refreshSeries();
}
export function setZoom(z) { state.zoom = z; bus.emit("range"); }
/** Pauses or resumes. Resuming a fixed window goes back to the last `range` minutes, live. */
export function setPaused(p) {
  if (!p && state.from != null) { state.paused = false; provide({ paused: false }); setRange(state.range); bus.emit("paused"); emitAll(); return; }
  state.paused = p; state.pausedAt = p ? Date.now() : null;
  provide({ paused: p });
  bus.emit("paused");
  if (!p) { emitAll(); refreshSeries(); }
}
export const last = () => state.points[state.points.length - 1];

/** Reads the range from a route's params (main.js calls it on every route); true when it changed. */
export function rangeFromUrl(params) {
  const from = +params.get("from"), to = +params.get("to"), key = params.get("range");
  if (from > 0 && to > from) {
    if (state.from === from && state.to === to) return false;
    const was = seriesKey();
    state.from = from; state.to = to; state.zoom = null;
    state.paused = true; state.pausedAt = to; provide({ paused: true });
    bus.emit("paused"); bus.emit("range");
    if (seriesKey() !== was) refreshSeries();
    return true;
  }
  if (key && rangeMin(key)) {
    if (state.from == null && state.range === rangeMin(key)) return false;
    setRange(key);
    return true;
  }
  // No range in the link: keep the one in use, and say it in the URL.
  writeUrl();
  return false;
}
/** Puts the range in the address bar without a new history entry. */
function writeUrl() {
  const raw = location.hash.replace(/^#\/?/, "");
  const [path, q] = raw.split("?");
  const p = new URLSearchParams(q || "");
  p.delete("range"); p.delete("from"); p.delete("to");
  for (const [k, v] of Object.entries(rangeParams())) p.set(k, String(v));
  const qs = p.toString();
  const next = "#/" + (path || "") + (qs ? "?" + qs : "");
  if (next !== location.hash && !(next === "#/" && !location.hash)) { try { history.replaceState(null, "", location.pathname + location.search + next); } catch { /* old browser */ } }
}

// ---------------------------------------------------------------- freshness
const PERIOD = { health: 5000, cluster: 5000, series: 2000, fleet: 5000, alerts: 5000 };
const fr = {};
function good(name) { fr[name] = { ok_ms: Date.now(), misses: 0, error: null, since_ms: null }; }
function bad(name, e) {
  const f = fr[name] || { ok_ms: null };
  fr[name] = { ok_ms: f.ok_ms, misses: (f.misses || 0) + 1, error: e?.message || String(e), status: e?.status ?? null, since_ms: f.since_ms || Date.now() };
}
/**
 * How fresh a panel's data is: fresh("health" | "cluster" | "series" | "fleet" | "alerts")
 * → { ok, age_ms, error, since_ms, loading }. Not ok after two missed polls
 * or when the server does not answer; age_ms is the time since the last
 * good answer (null if none yet). Views add the `stale` class to a card
 * that is not ok, and say its age.
 */
export function fresh(name) {
  const f = fr[name];
  if (!f) return { ok: !state.paused, age_ms: null, error: null, since_ms: null, loading: true };
  const age = f.ok_ms ? Date.now() - f.ok_ms : null;
  // Nobody at the page, the polls run IDLE_SLOW times less often (core.every): not stale for that.
  const per = ((name === "series" && state.series.source === "load") ? 60000 : PERIOD[name] || 5000) * (isIdle() ? IDLE_SLOW : 1);
  const down = !connection().ok;
  const ok = !down && (f.misses || 0) < 2 && (state.paused || age == null || age < per * 3 + 4000);
  return { ok, age_ms: age, error: f.error || (down ? connection().message : null), since_ms: f.since_ms || (down ? connection().since_ms : null), loading: false };
}
/** "data from 42 s ago" for a stale panel, or "" when fresh. */
export function staleText(name) {
  const f = fresh(name);
  if (f.ok || f.loading) return "";
  return f.age_ms == null ? "no data yet" : `data from ${fmt.ago(Date.now() - f.age_ms)}`;
}

// ---------------------------------------------------------------- polls
const T = { timeout: 15000 };
let seriesWindow = null; // the from/to of the window last read, for windows over an hour
const seriesKey = () => state.from != null ? `w${state.from}-${state.to}` : state.range <= MEMORY_MIN ? "live" : `r${state.range}`;
let seriesLoadedKey = null, seriesBusy = false;

async function pollSeries() {
  if (seriesBusy || !polls.series) { if (!polls.series) good("series"); return; }
  seriesBusy = true;
  try { await readSeries(); good("series"); }
  catch (e) { bad("series", e); bus.emit("fresh"); throw e; }
  finally { seriesBusy = false; }
}
async function readSeries() {
  const key = seriesKey();
  if (key === "live") {
    if (seriesLoadedKey !== "live") { state.points = []; seriesLoadedKey = "live"; }
    const since = state.points.length ? state.points[state.points.length - 1].t : 0;
    const j = await api("/v1/timeseries?since=" + since, T);
    if (seriesKey() !== "live") return;
    state.interval = j.interval_ms || 2000;
    state.series = { source: "memory", interval_ms: state.interval, from: null, to: null, partial: false, loaded_ms: Date.now() };
    if (j.points?.length) {
      state.points.push(...j.points);
      if (state.points.length > 1800) state.points.splice(0, state.points.length - 1800);
    }
    if (!state.paused) { bus.emit("points"); rederive(); }
    return;
  }
  // A window: read it whole, again now and then while it follows now.
  const r = timeRange();
  const from = Math.round(state.from ?? r.from), to = Math.round(state.to ?? r.to);
  const every = clampN((to - from) / 360, 30e3, 300e3);
  if (seriesLoadedKey === key && (state.from != null || Date.now() - (state.series.loaded_ms || 0) < every)) return;
  const j = await api(`/v1/timeseries?from=${from}&to=${to}`, T);
  if (seriesKey() !== key) return;
  // An older server answers its in-memory hour whatever it is asked.
  const ps = (j.points || []).filter((p) => p.t >= from - 60e3 && p.t <= to + 60e3);
  state.points = ps;
  state.interval = j.interval_ms || (ps.length > 1 ? Math.round((ps[ps.length - 1].t - ps[0].t) / (ps.length - 1)) : 2000);
  const first = ps.length ? ps[0].t : null;
  state.series = { source: j.source || "memory", interval_ms: state.interval, from, to, partial: !j.source && (!first || first - from > 5 * 60e3), loaded_ms: Date.now() };
  seriesWindow = [from, to];
  seriesLoadedKey = key;
  bus.emit("points");
}
const clampN = (v, a, b) => Math.max(a, Math.min(b, v));
/** Reads the series for the current range now (paused or not). */
export async function refreshSeries() { seriesLoadedKey = seriesKey() === "live" ? seriesLoadedKey : null; try { await pollSeries(); } catch { /* fresh() says it */ } }

async function pollHealth() {
  let h;
  try { h = await api("/v1/health", T); }
  catch (e) {
    // The server answers 503 with its health when shards are down: that is an answer.
    if (e.answered && e.body && typeof e.body.status === "string" && (e.body.role || e.body.shards || e.body.docs != null)) h = e.body;
    else { bad("health", e); setOnline(false, e.message); throw e; }
  }
  good("health");
  setOnline(true);
  if (state.paused) return;
  state.health = h;
  bus.emit("health");
}
function setOnline(on, error) {
  if (state.online === on && (on || state.error === error)) return;
  state.online = on; state.error = on ? null : error || connection().message;
  bus.emit("online");
}

async function pollAlerts() {
  if (polls.alerts === "slow" && Date.now() - alertsAt < 15000 && state.alerts) { good("alerts"); return; }
  alertsAt = Date.now();
  // alerts2: for the bell alone, the brief answer (a fifth of the whole).
  try { const a = await api(polls.alerts === "slow" ? "/v1/alerts?brief=1" : "/v1/alerts", T); good("alerts"); if (state.paused) return; state.alerts = a; bus.emit("alerts"); }
  catch (e) { if (e.status === 404) { good("alerts"); return; } bad("alerts", e); }
}

// What the view in use needs polled (SH-11): its cluster in full (each
// copy's detail) or brief (which copies answer, for the badges, every
// 15 s), and the series or not. main.js sets it on every route.
// The alerts too: every 5 s where they show, every 15 s for the bell elsewhere.
const polls = { cluster: "full", series: true, alerts: "full" };
let clusterAt = 0, alertsAt = 0;
export function setPolls(p) {
  const was = { ...polls };
  polls.cluster = p.cluster === "brief" ? "brief" : "full";
  polls.series = p.series !== false;
  polls.alerts = p.alerts === "slow" ? "slow" : "full";
  if (was.cluster === "brief" && polls.cluster === "full") { clusterAt = 0; pollCluster(); }
  if (was.alerts === "slow" && polls.alerts === "full" && (Date.now() - alertsAt > 4000 || state.alerts?.brief)) pollAlerts();
  if (!was.series && polls.series) refreshSeries();
}
async function pollCluster() {
  const brief = polls.cluster === "brief";
  if (brief && Date.now() - clusterAt < 15000 && state.cluster) { good("cluster"); return; }
  clusterAt = Date.now();
  let c;
  try { c = await api(brief ? "/v1/cluster?brief=1" : "/v1/cluster", T); }
  catch (e) { if (e.status === 404 || e.status === 400) { good("cluster"); return; } bad("cluster", e); bus.emit("fresh"); return; }
  good("cluster");
  if (state.paused && state.cluster) return;
  // Calls per second to each shard, from the counters of two polls.
  const prev = state.cluster?.shards ? Object.fromEntries(state.cluster.shards.map((x) => [x.url, x])) : {};
  for (const x of c.shards || []) {
    const n = (x.calls?.ok || 0) + (x.calls?.error || 0) + (x.calls?.timeout || 0);
    const p = prev[x.url];
    x.calls_total = n; x.polled_ms = Date.now();
    x.call_rate = p && x.polled_ms > p.polled_ms ? Math.max(0, (n - p.calls_total) / ((x.polled_ms - p.polled_ms) / 1000)) : null;
  }
  state.cluster = c;
  provide({ cluster: c });
  bus.emit("cluster");
}

let fleetMissing = 0; // when the server said it has no /v1/health/fleet
let derivedAt = 0;
/** Without /v1/health/fleet the fleet's health follows the points, at most every 4 s. */
function rederive() {
  if (!fleetMissing || state.paused || Date.now() - derivedAt < 4000) return;
  derivedAt = Date.now();
  state.fleet = deriveFleet(); bus.emit("fleet");
}
async function pollFleet() {
  // Worked out here, the fleet's health is as fresh as the cluster it comes from.
  if (fleetMissing && Date.now() - fleetMissing < 120e3) { if (fresh("cluster").ok) good("fleet"); if (!state.paused || !state.fleet) { derivedAt = Date.now(); state.fleet = deriveFleet(); bus.emit("fleet"); } return; }
  try {
    const f = await api("/v1/health/fleet", T);
    fleetMissing = 0; good("fleet");
    if (state.paused && state.fleet) return;
    state.fleet = f;
  } catch (e) {
    if (e.status === 404 || e.status === 405) { fleetMissing = Date.now(); good("fleet"); state.fleet = deriveFleet(); }
    // A key limited to tables does not see the fleet: said once, not polled again (core.api).
    else if (e.status === 403 && e.body?.limited) { state.fleetRefused = e.message; good("fleet"); }
    else { bad("fleet", e); bus.emit("fresh"); return; }
  }
  bus.emit("fleet");
}

export async function refreshSystem() {
  try { state.system = await api("/v1/system", T); bus.emit("system"); } catch { /* shown by the views */ }
}
let routesFor = null;
export async function refreshWho() {
  try { state.whoami = await api("/v1/whoami", T); } catch { state.whoami = null; }
  // A key for operations (ops, propose) acts by route: the catalog says
  // which routes its role may call, read once a key.
  const role = roleOf(state.whoami);
  if (role && routesFor !== state.whoami.key) {
    try { provideRoutes((await api("/v1/catalog", T)).routes); routesFor = state.whoami.key; } catch { provideRoutes(null); routesFor = null; }
  }
  provide({ whoami: state.whoami });
  bus.emit("whoami");
}
export const refreshCluster = pollCluster;
export const refreshFleet = pollFleet;
export async function refreshAlerts() { try { state.alerts = await api("/v1/alerts"); good("alerts"); bus.emit("alerts"); } catch { /* shown by the views */ } }

function emitAll() { for (const ev of ["points", "health", "cluster", "fleet", "alerts", "range"]) bus.emit(ev); }

// ---------------------------------------------------------------- alerts
/**
 * The alerts firing now, from either shape of /v1/alerts (the older one
 * lists the firing rules; the newer one counts them and has `states`):
 * [{ id, name, severity, since_ms, value, where: [{copy, short, ...}], ack, silenced }].
 */
export function firingAlerts(a = state.alerts) {
  if (!a) return [];
  const rules = a.rules || [];
  if (Array.isArray(a.firing)) return a.firing.map((r) => ({ id: r.id, name: r.name, severity: r.severity, since_ms: r.status?.since_ms || null, value: r.status?.value ?? null, where: r.status?.where || [], ack: r.status?.ack || null, silenced: r.status?.silenced || null, description: r.description }));
  return (a.states || []).filter((x) => x.state === "firing").map((x) => {
    const r = rules.find((y) => y.id === x.rule) || {};
    return { id: x.rule, name: r.name || x.name || x.rule, severity: r.severity || x.severity || "warning", since_ms: x.since_ms || null, value: x.value ?? null, where: x.where || [], ack: x.ack || null, silenced: x.silenced || null, held_by: x.held_by || null, description: r.description, on: firesOn(x) };
  });
}

/**
 * The other coordinators an alert fires on when it does not fire on this
 * one (its rule judges each coordinator's own traffic, R2-scenarios-1):
 * "coordinator-b", or null when it fires here.
 */
export function firesOn(x) {
  if (!x?.here || !Array.isArray(x.elsewhere) || !x.elsewhere.length) return null;
  const host = (u) => { try { return new URL(u).host; } catch { return String(u || ""); } };
  const there = x.elsewhere.filter((e) => e.state === x.state);
  return (there.length ? there : x.elsewhere).map((e) => e.name || host(e.url)).join(", ");
}

/** The alerts firing now that nobody silenced nor a cause holds back (alerts2): the one count the bell, Alerts and Home show. */
export const firingNow = () => firingAlerts().filter((a) => !a.silenced && !a.held_by);

/**
 * The fleet's problems as Home, its banner and the navigation show them
 * (LIV-9): the server's last answer, and the alerts firing now (the bell's
 * count) when that answer did not count them yet, as the server would: an
 * info alert tells but is no problem. Worst first.
 */
export function homeProblems(f = state.fleet) {
  const probs = [...(f?.problems || [])];
  const told = firingNow().filter((a) => a.severity !== "info");
  if (told.length && !probs.some((p) => p.id === "alerts" || String(p.id || "").startsWith("alert-"))) {
    const crit = told.filter((a) => a.severity === "critical").length;
    const on = (a) => (a.on ? ` on ${a.on}` : a.where?.length === 1 ? ` on ${a.where[0].short || a.where[0].copy || a.where[0]}` : a.where?.length > 1 ? ` on ${a.where.length} copies` : "");
    const since = told.map((a) => a.since_ms).filter(Boolean);
    probs.push({
      id: "alerts", severity: crit ? "critical" : "warning",
      title: `${told.length} ${told.length === 1 ? "alert is" : "alerts are"} firing${crit ? `, ${crit} critical` : ""}: ${told.slice(0, 2).map((a) => a.name + on(a)).join("; ")}${told.length > 2 ? ` and ${told.length - 2} more` : ""}`,
      where: [], since_ms: since.length ? Math.min(...since) : null,
      impact: told.slice(0, 4).map((a) => `${a.name} (${a.severity})`).join(", ") + (told.length > 4 ? ` and ${told.length - 4} more` : ""),
      next: told.length === 1 ? { text: "Open the alert", view: "alert/" + encodeURIComponent(told[0].id), params: {} } : { text: "Open Alerts", view: "alerts", params: {} },
    });
    probs.sort((a, b) => (a.severity === "critical" ? 0 : 1) - (b.severity === "critical" ? 0 : 1));
  }
  return probs;
}
/** The status Home shows: the server's, or worse when an alert fires that its last answer did not count. */
export function homeStatus(f = state.fleet) {
  const probs = homeProblems(f);
  const st = f?.status || "ok";
  if (probs.some((p) => p.severity === "critical")) return "critical";
  return st === "ok" && probs.length ? "warning" : st;
}

// ---------------------------------------------------------------- the cluster's identity
const idOf = () => state.cluster?.cluster || state.whoami?.cluster || state.health?.cluster || null;
/** The cluster's name ("prod/search"), or null when the server does not say. */
export const clusterName = () => idOf()?.name || null;
/** The environment ("production", "staging"...), or null when unknown. */
export const environment = () => idOf()?.environment || null;
/**
 * Whether to treat this cluster as production for safety (read-only
 * workbench, typed confirms): true for an environment that starts with
 * "prod", and true when the environment is unknown. The PROD badge shows
 * only for a known production environment (`environment()`).
 */
// NEW-data-tools-13: production is anything not named as a lower environment (prd, live, production-eu).
export const isProduction = () => { const e = environment(); return e == null ? !isSandbox() : !/^(dev|development|test|testing|stage|staging|qa|local|sandbox|demo)\b/i.test(e); };
/**
 * A sandbox: a server with no environment set, opened on this machine
 * (localhost), not through a hub: someone trying ChloeDB, whose writes
 * are on without a reason asked (each still shows the rows it changes). Naming an environment (--environment) ends it.
 */
export const isSandbox = () => environment() == null && /^(localhost|127(\.\d{1,3}){3}|\[::1\]|::1)$/i.test(location.hostname) && !/^\/c\//.test(location.pathname);

// ---------------------------------------------------------------- the fleet without /v1/health/fleet
/**
 * A shard's state: the server's `state` when it says ("ok", "degraded",
 * "no_reads", "no_writes", "down"), else worked out from its copies: with
 * failover a primary needs a majority of its copies to serve reads.
 */
export const shardState = (sh) => {
  if (sh.state) return sh.state;
  const reps = sh.replicas || [];
  const up = reps.filter((r) => (r.status === "ok" || r.status == null) && r.role !== "unknown").length;
  if (!reps.length || up === 0 || sh.status === "down") return "down";
  // With failover, a primary needs a majority of its copies to serve reads.
  const failover = sh.failover ?? reps.length > 1;
  if (failover && up * 2 <= reps.length) return "no_reads";
  return up < reps.length || sh.status === "degraded" ? "degraded" : "ok";
};
/**
 * The contract's fleet health, worked out from /v1/cluster, /v1/health,
 * /v1/alerts and the points, for a server without /v1/health/fleet.
 */
export function deriveFleet() {
  const c = state.cluster, hl = state.health, now = Date.now();
  const ps = state.points.filter((p) => p.t >= now - 300e3);
  const mean = (f) => { const v = ps.map(f).filter((x) => x != null && Number.isFinite(x)); return v.length ? v.reduce((a, b) => a + b, 0) / v.length : null; };
  const qsum = ps.reduce((a, p) => a + (p.qps || 0) * (p.secs || 2), 0);
  const esum = ps.reduce((a, p) => a + (p.errors_s || 0) * (p.secs || 2), 0);
  const errRatio = qsum > 0 ? esum / qsum : null;
  const p99 = ps.length ? Math.max(...ps.map((p) => p.p99_ms || 0)) : null;
  const sloErr = 0.01, sloP99 = 250;
  const checks = [], problems = [];
  const shards = c?.shards || [];
  const name = (url) => {
    for (let i = 0; i < shards.length; i++) { const j = (shards[i].replicas || []).findIndex((r) => r.url === url); if (j >= 0) return shards[i].replicas[j].name || shards[i].replicas[j].short || `s${shards[i].shard ?? i}-${j}`; }
    return url;
  };
  const counts = { no_reads: 0, no_writes: 0, degraded: 0, down: 0, total: shards.length };
  const where = [];
  const downCopies = [];
  let total = 0, maxLag = null, maxDisk = null, disks = [];
  shards.forEach((sh, i) => {
    const st = shardState(sh);
    if (st !== "ok") { counts[st] = (counts[st] || 0) + 1; where.push("shard " + (sh.shard ?? i)); }
    (sh.replicas || []).forEach((r, j) => {
      total++;
      if (r.status && r.status !== "ok" || r.role === "unknown") downCopies.push({ name: r.name || r.short || `s${sh.shard ?? i}-${j}`, shard: sh.shard ?? i, since: r.down_since_ms });
      const L = r.detail?.load || {};
      if (L.replica_behind_max != null && (maxLag == null || L.replica_behind_max > maxLag.v)) maxLag = { v: L.replica_behind_max, where: r.name || r.short || `s${sh.shard ?? i}-${j}` };
      if (L.disk_used_ratio != null) { disks.push(L.disk_used_ratio); if (!maxDisk || L.disk_used_ratio > maxDisk.v) maxDisk = { v: L.disk_used_ratio, where: r.name || r.short || `s${sh.shard ?? i}-${j}` }; }
    });
    if (st === "no_reads" || st === "down" || st === "no_writes") {
      const reps = sh.replicas || [];
      const up = reps.filter((r) => r.status === "ok").length;
      // "No majority" only when fewer than a majority answer: a shard whose
      // primary went with a majority left has no primary until one is
      // elected (finding chaos-2).
      const n = sh.shard ?? i, majority = sh.majority ?? up * 2 > reps.length;
      const title = st === "down" ? `Shard ${n} does not answer`
        : !majority ? `Searches are failing: shard ${n} has no majority`
        : st === "no_writes" ? `Writes are failing: shard ${n} ${sh.primary ? "takes no writes" : "has no primary"}`
        : `Searches are failing: shard ${n} serves no reads`;
      const gone = reps.filter((r) => r.status !== "ok");
      const text = !majority ? (gone.length === 1 ? `Bring ${name(gone[0].url)} back, or remove it so the others have a majority` : "Bring its copies back")
        : st === "no_writes" && !sh.primary ? "An election should choose a primary within seconds. If none does, make an up-to-date copy the primary (Cluster: its menu)"
        : st === "no_writes" ? "Hand the primary over to a replica in sync (Cluster: its menu, Make it the primary), then fix the old primary"
        : "Find out why on its copies";
      problems.push({ id: "shard-" + n, severity: "critical", title,
        where: ["shard " + n, ...gone.map((r) => name(r.url))], since_ms: null,
        impact: sh.why || `${up} of ${reps.length} copies answer: without a majority the shard serves no reads, and every search asks every shard`,
        next: { text, view: "cluster", params: { shard: n } } });
    }
  });
  disks.sort((a, b) => a - b);
  const median = disks.length ? disks[Math.floor(disks.length / 2)] : null;
  const sev = (bad, warn) => (bad ? "critical" : warn ? "warning" : "ok");
  if (shards.length) checks.push({ id: "shards", status: sev(counts.no_reads + counts.down + counts.no_writes > 0, counts.degraded > 0), value: counts, where });
  checks.push({ id: "error_ratio", status: errRatio == null ? "ok" : sev(errRatio > sloErr, errRatio > sloErr / 2), value: errRatio, slo: sloErr, window_s: 300 });
  checks.push({ id: "p99", status: p99 == null ? "ok" : sev(p99 > sloP99, p99 > sloP99 * 0.8), value_ms: p99, slo_ms: sloP99, window_s: 300 });
  if (shards.length) checks.push({ id: "copies", status: sev(false, downCopies.length > 0), down: downCopies.length, total, where: downCopies.map((d) => d.name) });
  if (!shards.length && hl && hl.status && hl.status !== "ok") checks.push({ id: "server", status: hl.status === "degraded" ? "warning" : "critical", value: hl.status, where: [location.host] });
  if (maxLag) checks.push({ id: "lag", status: sev(maxLag.v > 100000, maxLag.v > 1000), max_txn: maxLag.v, where: maxLag.where });
  if (maxDisk) checks.push({ id: "disk", status: sev(maxDisk.v > 0.9, maxDisk.v > 0.8), max: maxDisk.v, median, where: maxDisk.where });
  const firing = firingAlerts();
  if (state.alerts) checks.push({ id: "alerts", status: sev(firing.some((a) => a.severity === "critical"), firing.length > 0), firing: firing.length, critical: firing.filter((a) => a.severity === "critical").length });
  if (downCopies.length && !problems.some((p) => p.id.startsWith("shard-"))) problems.push({ id: "copies-down", severity: "warning", title: downCopies.length === 1 ? `${downCopies[0].name} does not answer` : `${downCopies.length} copies do not answer`, where: downCopies.map((d) => d.name), since_ms: downCopies[0].since || null, impact: "The other copies of these shards still serve", next: { text: "See the copies in Cluster", view: "cluster", params: {} } });
  if (errRatio != null && errRatio > sloErr) problems.push({ id: "error-ratio", severity: "critical", title: `${fmt.pct(errRatio, 1)} of queries fail (SLO ${fmt.pct(sloErr, 0)})`, where: [], since_ms: null, impact: `over the last 5 min, on this coordinator`, next: { text: "See the failing queries", view: "queries", params: {} } });
  if (p99 != null && p99 > sloP99) problems.push({ id: "p99", severity: "warning", title: `p99 ${fmt.ms(p99)} is over the SLO (${fmt.ms(sloP99)})`, where: [], since_ms: null, impact: "over the last 5 min, on this coordinator", next: { text: "See where the time goes", view: "performance", params: {} } });
  if (maxDisk && maxDisk.v > 0.8) problems.push({ id: "disk", severity: maxDisk.v > 0.9 ? "critical" : "warning", title: `${maxDisk.where} is ${fmt.pct(maxDisk.v, 0)} full`, where: [maxDisk.where], since_ms: null, impact: "writes stop when a disk is full", next: { text: "See capacity", view: "cluster", params: {} } });
  for (const a of firing) problems.push({ id: "alert-" + (a.id || a.name), severity: a.severity === "critical" ? "critical" : "warning", title: `Alert: ${a.name}`, where: a.where ? [].concat(a.where).map((w) => w.short || w.copy || w) : [], since_ms: a.since_ms || null, impact: a.description || "", next: { text: "Open the alert", view: "alert/" + (a.id || a.rule || a.name), params: {} } });
  const order = { critical: 0, warning: 1 };
  problems.sort((a, b) => order[a.severity] - order[b.severity]);
  const worst = problems[0];
  const status = checks.some((x) => x.status === "critical") ? "critical" : checks.some((x) => x.status === "warning") ? "warning" : "ok";
  return {
    derived: true, at_ms: now, status, headline: worst ? worst.title : status === "ok" ? "Every check is within its limit" : "Something needs a look",
    since_ms: worst?.since_ms || null, api: { ok: connection().ok }, checks, problems,
    coordinators: hl?.role === "coordinator" || c ? [{ url: location.origin, name: location.host, qps: mean((p) => p.qps), p99_ms: p99, error_ratio: errRatio, cpu: mean((p) => p.cpu), this: true }] : [],
  };
}

// ---------------------------------------------------------------- how the cluster is run
// LIV-10: an operator-managed cluster takes its settings from its
// StrataCluster's spec, not from flags: advice names what to change where
// it is changed. GET /v1/kube, asked once (a minute at most) by a view
// that gives advice; "kube" says it changed.
let kubeAt = 0;
export function kubeInfo() {
  if (Date.now() - kubeAt > 60e3) { kubeAt = Date.now(); api("/v1/kube", T).then((k) => { state.kube = k; bus.emit("kube"); }, () => { state.kube = state.kube || { managed: false }; }); }
  return state.kube || null;
}
/** Whether an operator manages this cluster (its spec holds its settings). */
export const managed = () => !!kubeInfo()?.managed;
/** The StrataCluster, as people name it: "ns/name". */
export const specName = () => (state.kube?.managed ? `${state.kube.namespace}/${state.kube.name}` : null);
/**
 * How to set something: with the operator, the spec field
 * ("spec.environment"), else the server's flag ("--environment"). Returns
 * text: "set spec.environment: production (or staging) in the StrataCluster
 * ns/name" or "start the server with --environment production (or staging)".
 */
export function howToSet({ flag, field, value = "", or = "" }) {
  const alt = or ? ` (or ${or})` : "";
  if (managed() && field) return `set ${field}${value ? ": " + value : ""}${alt} in the StrataCluster ${specName()}`;
  return `start the server with ${flag}${value ? " " + value : ""}${alt}`;
}

// ---------------------------------------------------------------- proposals waiting
// The on-call agent's proposals and change requests that wait for an admin:
// the navigation counts them, whichever view is open.
let pendingN = 0;
export const pendingProposals = () => pendingN;
export function setPendingProposals(n) { pendingN = n || 0; bus.emit("pending"); }
async function pollPending() {
  if (!can("admin", "GET /v1/admin/oncall/proposals")) { pendingN = 0; return; }
  try { setPendingProposals((await api("/v1/admin/oncall/proposals?state=pending", T)).proposals?.length); } catch { /* an older server, or no key */ }
}
bus.on("whoami", () => { pendingN = 0; pollPending(); });

// ---------------------------------------------------------------- start
let stops = [];
export function start() {
  stops.forEach((f) => f());
  stops = [
    every(2000, pollSeries),
    every(5000, pollHealth, true, { always: true, idle: "slow" }),
    every(5000, pollCluster),
    every(5000, pollAlerts),
    every(5000, pollFleet),
    every(30000, refreshSystem),
    every(20000, pollPending, false),
  ];
  return refreshWho();
}
provide({ rangeParams: () => rangeParams() });
bus.on("token", () => { state.points = []; seriesLoadedKey = null; start(); });
bus.on("connection", (c) => { if (!c.ok) setOnline(false, c.message); bus.emit("fresh"); });
