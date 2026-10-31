// What every view reads, fetched once for all of them: the series of
// points (every 2 s, an hour of it), the server's health, system, alerts
// and cluster, and who the viewer is. Views listen on the bus.

import { api, bus, every, store } from "./core.js";

export const state = {
  range: store.get("range", 15), // minutes shown
  zoom: null, // [t0, t1] picked with a brush
  paused: false,
  points: [],
  interval: 2000,
  health: null, system: null, alerts: null, cluster: null, whoami: null,
  online: true, error: null,
};

export const isCoordinator = () => state.health?.role === "coordinator" || state.system?.backend === "coordinator";

/** The span of time the charts show. */
export function domain() {
  if (state.zoom) return state.zoom;
  const t1 = state.points.length ? state.points[state.points.length - 1].t : Date.now();
  // While the series is younger than the range, it fills the width.
  const first = state.points.length ? state.points[0].t : t1;
  return [Math.max(t1 - state.range * 60e3, Math.min(first, t1 - 60e3)), t1];
}
export function visible() {
  const [a, b] = domain();
  return state.points.filter((p) => p.t >= a - state.interval && p.t <= b + state.interval);
}
export function setRange(min) { state.range = min; state.zoom = null; store.set("range", min); bus.emit("range"); }
export function setZoom(z) { state.zoom = z; bus.emit("range"); }
export function setPaused(p) { state.paused = p; bus.emit("paused"); if (!p) bus.emit("points"); }
export const last = () => state.points[state.points.length - 1];

async function pollSeries() {
  const since = state.points.length ? state.points[state.points.length - 1].t : 0;
  const j = await api("/v1/timeseries?since=" + since);
  state.interval = j.interval_ms || 2000;
  if (j.points?.length) {
    state.points.push(...j.points);
    if (state.points.length > 1800) state.points.splice(0, state.points.length - 1800);
    if (!state.paused) bus.emit("points");
  }
}

async function pollHealth() {
  try {
    state.health = await api("/v1/health");
    if (!state.online) { state.online = true; bus.emit("online"); }
  } catch (e) {
    if (state.online || state.error !== e.message) { state.online = e.status != null; state.error = e.message; bus.emit("online"); }
    throw e;
  }
  bus.emit("health");
  try { state.alerts = await api("/v1/alerts"); bus.emit("alerts"); } catch { /* an older server */ }
}

async function pollCluster() {
  try {
    const c = await api("/v1/cluster");
    // Calls per second to each shard, from the counters of two polls.
    const prev = state.cluster?.shards ? Object.fromEntries(state.cluster.shards.map((x) => [x.url, x])) : {};
    for (const x of c.shards || []) {
      const n = (x.calls?.ok || 0) + (x.calls?.error || 0) + (x.calls?.timeout || 0);
      const p = prev[x.url];
      x.calls_total = n; x.polled_ms = Date.now();
      x.call_rate = p && x.polled_ms > p.polled_ms ? Math.max(0, (n - p.calls_total) / ((x.polled_ms - p.polled_ms) / 1000)) : null;
    }
    state.cluster = c;
    bus.emit("cluster");
  } catch { /* shown by the views */ }
}

export async function refreshSystem() {
  try { state.system = await api("/v1/system"); bus.emit("system"); } catch { /* shown by the views */ }
}
export async function refreshWho() {
  try { state.whoami = await api("/v1/whoami"); } catch { state.whoami = null; }
  bus.emit("whoami");
}
export const refreshCluster = pollCluster;
export async function refreshAlerts() { try { state.alerts = await api("/v1/alerts"); bus.emit("alerts"); } catch { /* shown by the views */ } }

let stops = [];
export function start() {
  stops.forEach((f) => f());
  stops = [
    every(2000, pollSeries),
    every(5000, pollHealth),
    every(5000, pollCluster),
    every(30000, refreshSystem),
  ];
  refreshWho();
}
bus.on("token", () => { state.points = []; start(); });
