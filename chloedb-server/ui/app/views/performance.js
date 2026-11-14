// Performance: the fleet over the range in the top bar. What needs
// attention, each finding with the numbers it rests on, the rule that
// fired and links to the statement or the copy; the latency, the queries
// and the failures stacked by cause; every copy by p99; the statements
// that got slower; and, below, queries by kind and route, writes, this
// server's resources and every copy's storage and replication.

import { h, s, icon, clear, fmt, card, api, bus, store, table, copy, seg } from "../core.js";
import { timeChart, heatmap, SERIES, sparkline } from "../charts.js";
import { domain, visible, setZoom } from "../data.js";
import {
  state, link, timeRange, rangeQuery, poll, isCoord, copyName, copyTitle, copyLink, shardLink, nodeLink, statements, change, split, outcomeBar, statementLink, isOwn, containDialog, can,
  narrow, cols, sheet, copies as copiesOf,
  canContainNow, canContain, containRules, when,
  ask, objects, hasLogStore, queryColumns, causesAndVictims,
} from "../workload.js";

export default { id: "performance", title: "Performance", icon: "perf", group: "Operate", keywords: "latency throughput heatmap findings advisor tuning cpu memory disk routes kinds percentiles slow copies fleet failures", render, onParams };

const BOUNDS = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0];
/** The slow bands one can ask about: over 100 ms, 1 s, 10 s. */
const BANDS = [[100, "100 ms"], [1000, "1 s"], [10000, "10 s"]];
const TABS = [["queries", "Kinds and routes"], ["writes", "Writes"], ["resources", "Resources"], ["storage", "Storage"], ["replication", "Replication"]];

// ---------------------------------------------------------------- figures from the series

const stepS = (p) => p.secs || (state.series?.interval_ms || state.interval || 2000) / 1000;
/** The time-weighted mean of a field over points, or null when no point has it. */
function mean(ps, f) { let a = 0, w = 0; for (const p of ps) { const v = f(p); if (v == null || !Number.isFinite(v)) continue; const s_ = stepS(p); a += v * s_; w += s_; } return w ? a / w : null; }
/** How many over the points: a rate times each point's seconds. */
function total(ps, f) { let a = 0, any = false; for (const p of ps) { const v = f(p); if (v == null || !Number.isFinite(v)) continue; any = true; a += v * stepS(p); } return any ? a : null; }
const median = (xs) => { const v = xs.filter((x) => x != null && Number.isFinite(x)).sort((a, b) => a - b); return v.length ? v[Math.floor(v.length / 2)] : null; };
/** A stretch's p99, as the chart draws it: the median of its points' p99 (the histograms' buckets are too wide for it). */
const p99Of = (ps) => median(ps.map((p) => p.p99_ms));
/** The points minute by minute: [{t, p99, qps}]. */
function minutes(ps) {
  const m = new Map();
  for (const p of ps) { const k = Math.floor(p.t / 60e3) * 60e3; if (!m.has(k)) m.set(k, []); m.get(k).push(p); }
  return [...m].sort((a, b) => a[0] - b[0]).map(([t, xs]) => ({ t, p99: p99Of(xs), qps: mean(xs, (p) => p.qps) }));
}
/** The figures of a range of points. */
function figures(ps) {
  const q = total(ps, (p) => p.qps);
  const errs = total(ps, (p) => p.errors_s), tos = total(ps, (p) => p.timeouts_s);
  return {
    n: ps.length, qps: mean(ps, (p) => p.qps), p99: mean(ps, (p) => p.p99_ms), p99Now: p99Of(ps.filter((p) => p.t >= (ps[ps.length - 1]?.t ?? 0) - 5 * 60e3)), p50: mean(ps, (p) => p.p50_ms),
    queries: q, server: errs, timeouts: tos, client: total(ps, (p) => p.client_errors_s), deadlines: total(ps, (p) => p.client_deadlines_s), refused: total(ps, (p) => p.rejected_s),
    slow: mean(ps, (p) => p.slow_s), refusedS: mean(ps, (p) => p.rejected_s), splits: ps.some((p) => p.client_errors_s != null),
    cpu: mean(ps, (p) => p.cpu),
  };
}

// ---------------------------------------------------------------- every copy, from the cluster

// ---------------------------------------------------------------- the view

// A link's ?tab= opens that tab (and the address says the tab shown).
let onTab = null;
function onParams(r) { onTab?.(r.params.get("tab")); }
const tabOf = (k) => (TABS.some(([t]) => t === k) ? k : null);

function render(page, r) {
  let rules = null, stmts = null, stmtsErr = null, prev = null, prevKey = "", load = undefined, routes = null, routesErr = null, showOwnRoutes = false;
  // What runs here now, by statement (the memory finding names the longest).
  let liveGroups = null;
  const scope = h("p");
  const kpis = h("div", { class: "grid g6 wl-kpis", style: { marginBottom: "14px" } });
  const findBox = h("div", { style: { marginBottom: "14px" } });
  const brush = (a, b) => setZoom(a == null ? null : [a, b]);
  const charts = [];
  const C = (title, sub, opts) => { const c = timeChart({ height: opts.height || 180, domain, onBrush: brush, ...opts }); charts.push(c); return card({ title, sub, body: c }); };

  const latC = C("Latency", "of the queries that ended in each interval", { fmt: fmt.ms, series: [{ name: "p50", get: (p) => p.p50_ms, color: SERIES[0] }, { name: "p95", get: (p) => p.p95_ms, color: SERIES[3] }, { name: "p99", get: (p) => p.p99_ms, color: SERIES[6] }], band: { lo: (p) => p.p50_ms, hi: (p) => p.p99_ms, name: "p50 to p99" } });
  const failC = C("Failures per second, by cause", "red is the server's; client errors and deadlines are the clients'", { stack: true, series: [
    { name: "server errors", get: (p) => p.errors_s == null ? null : Math.max(0, p.errors_s - (p.timeouts_s || 0)), color: "var(--crit)" },
    { name: "server timeouts", get: (p) => p.timeouts_s, color: "var(--s8)" },
    { name: "partial answers", get: (p) => p.partial_s, color: "var(--s5)" },
    { name: "client errors (400)", get: (p) => p.client_errors_s, color: "var(--s4)" },
    { name: "client deadlines", get: (p) => p.client_deadlines_s, color: "var(--s6)" },
    { name: "refused (429)", get: (p) => p.rejected_s, color: "var(--s2)" },
  ] });
  const kindsList = () => { const k = new Set(); for (const p of visible()) for (const x of Object.keys(p.by_kind || {})) k.add(x); return [...k].sort(); };
  let kindsKey = "";
  const qpsHost = h("div");
  let qpsC = null;
  function qpsChart() {
    const ks = kindsList(), key = ks.join(",");
    if (qpsC && key === kindsKey) return;
    kindsKey = key;
    if (qpsC) charts.splice(charts.indexOf(qpsC), 1);
    qpsC = timeChart({ height: 180, domain, onBrush: brush, stack: true, fmt: fmt.si, series: (ks.length ? ks : ["queries"]).map((k, i) => ({ name: k, get: (p) => ks.length ? p.by_kind?.[k] || 0 : p.qps, color: k === "invalid" ? "var(--s4)" : SERIES[i % 8] })) });
    charts.push(qpsC);
    clear(qpsHost, qpsC);
  }
  let hm = heatmap({ bounds: BOUNDS, domain, height: 180, interval: state.interval }), hmInterval = state.interval;
  const hmNote = h("div", { class: "small muted", style: { display: "none", padding: "40px 0", textAlign: "center" } }, "The heatmap needs the 2 s points of the last hour: pick 1 h or less in the top bar.");
  const qpsCard = card({ title: "Queries per second, by kind", sub: "stacked", body: qpsHost });
  // PERF-3: who filled a slow band: the statements and the copies.
  const bandBox = h("div", { class: "wl-bands" });
  let band = store.get("perf.band", 1000), bandsKey = "", bands = undefined;
  const bandSeg = seg(BANDS.map(([v, l]) => [v, "over " + l]), band, (v) => { band = v; store.set("perf.band", v); bands = undefined; bandsKey = ""; drawBands(); loadBands(); },
    { title: "The statements and copies of the queries slower than this, in the range" });
  const hmCard = card({ title: "Latency heatmap", sub: "every query by how long it took · darker is more · below, who filled the slow bands", body: [hm, hmNote, bandBox] });

  const copiesBox = h("div");
  const slowerBox = h("div", { style: { margin: "14px 0" } });
  const tabs = h("div", { class: "tabs wl-tabs-scroll", role: "tablist", style: { marginTop: "6px" } });
  const tabBody = h("div");
  let tab = tabOf(r?.params?.get("tab")) || tabOf(store.get("perf.tab2", "queries")) || "queries";
  const showTab = (k) => {
    tab = k; store.set("perf.tab2", k);
    const q = new URLSearchParams(location.hash.split("?")[1] || "");
    q.set("tab", k);
    const want = location.hash.split("?")[0] + "?" + q;
    if (location.hash !== want) history.replaceState(null, "", want);
    drawTabs(); drawTab(); if (k === "queries") loadRoutes();
  };
  onTab = (k) => { if (tabOf(k) && k !== tab) showTab(k); };
  // PERF-6: what the KPIs compare with: the window before, or the same time a day or a week ago.
  const BASES = [["before", "window before", "the window before"], ["day", "yesterday", "the same time yesterday"], ["week", "last week", "the same time last week"]];
  let base = store.get("perf.base", "before"), prevNone = null;
  if (!BASES.some(([k]) => k === base)) base = "before";
  const baseWords = () => BASES.find(([k]) => k === base)[2];
  const baseSeg = seg(BASES.map(([v, l, t]) => [v, l, `Compare the numbers with ${t}`]), base, (v) => { base = v; store.set("perf.base", v); prevKey = ""; loadPrev(); },
    { "aria-label": "Compare with", class: "seg wl-base" });

  page.append(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Performance"), scope),
      h("div", { class: "tools" }, h("span", { class: "small dim al-wide-only" }, "Compare with"), baseSeg, h("button", { class: "btn sm", onclick: () => copy(location.href, "Link copied") }, icon("link"), "Copy link"))),
    kpis, findBox,
    h("div", { class: "grid g2", style: { marginBottom: "14px" } }, latC, failC, qpsCard, hmCard),
    copiesBox, slowerBox, tabs, tabBody);

  // ------------------------------------------------ reads
  async function loadStmts() {
    try { [stmts] = await Promise.all([statements(timeRange()), canContain()]); stmtsErr = null; } catch (e) { stmts = null; stmtsErr = e; }
    // The contain rules in force, to tell refusals on purpose from a full server.
    rules = canContainNow() ? await containRules().catch(() => null) : null;
    liveGroups = await api("/v1/queries?group=shape").then((j) => j.groups || null).catch(() => null);
    draw();
  }
  /** The window before the range, or the same window a day or a week ago, for "was" (only when the server still has it). */
  async function loadPrev() {
    const r = timeRange(), len = (r.to ?? Date.now()) - r.from;
    const shift = base === "day" ? 864e5 : base === "week" ? 7 * 864e5 : len;
    const from = r.from - shift, to = from + len;
    const key = `${base}|${Math.round(from / 60e3)}|${Math.round(to / 60e3)}`;
    if (key === prevKey) return;
    prevKey = key;
    if (len > shift) { prev = null; prevNone = `a range longer than ${base === "day" ? "a day" : "a week"} has no ${baseWords().slice(4)}`; draw(); return; }
    try {
      const j = await api(`/v1/timeseries?from=${Math.round(from)}&to=${Math.round(to)}`);
      const ps = (j.points || []).filter((p) => p.t >= from && p.t <= to);
      const covered = ps.length > 1 ? (ps[ps.length - 1].t - ps[0].t) / len : 0;
      prev = covered >= 0.5 ? figures(ps) : null;
      prevNone = prev ? null : `${isCoord() ? "this coordinator" : "this server"} kept no points from ${baseWords()}${j.source === "memory" && !state.system?.log_store ? " (without a log store it keeps the last hour)" : ""}`;
    } catch { prev = null; prevNone = `${baseWords()} could not be read`; }
    if (key === prevKey) draw();
  }
  async function loadLoad() {
    if (!isCoord()) { load = null; return; }
    try { load = await api(`/v1/load?${rangeQuery()}&copies=all`); } catch (e) { load = e.status === 404 || e.status === 405 || e.status === 400 ? null : undefined; }
    draw();
  }
  /** The statements and copies of the queries slower than the band, from the log store. */
  async function loadBands() {
    const r = timeRange(), key = `${band}|${Math.round(r.from / 30e3)}|${r.live ? "now" : Math.round((r.to ?? 0) / 30e3)}`;
    if (key === bandsKey) return;
    bandsKey = key;
    if (!(await hasLogStore().catch(() => false))) { bands = null; drawBands(); return; }
    const lit = (v) => `'${String(v).replace(/'/g, "''")}'`;
    const cols = await queryColumns().catch(() => new Set());
    const w = [`ts >= ${lit(new Date(r.from).toISOString())}`, !r.live && r.to ? `ts < ${lit(new Date(r.to).toISOString())}` : null, `ms >= ${band}`,
      cols.has("client") ? "(client IS NULL OR client != 'command-center')" : null].filter(Boolean).join(" AND ");
    try {
      const [st, cp] = await Promise.all([
        ask(`SELECT fingerprint, COUNT(*) AS n, MAX(ms) AS slowest FROM queries WHERE ${w} AND shard = false GROUP BY fingerprint ORDER BY n DESC LIMIT 8`),
        isCoord() ? ask(`SELECT node, COUNT(*) AS n, MAX(ms) AS slowest FROM queries WHERE ${w} AND shard = true GROUP BY node ORDER BY n DESC LIMIT 8`) : null]);
      if (key !== bandsKey) return;
      bands = { st: objects(st), cp: cp ? objects(cp) : null };
    } catch (e) { if (key === bandsKey) bands = { err: e }; }
    drawBands();
  }
  async function loadRoutes() {
    // Over the range; a server older than windowed routes answers since it started.
    try { routes = await api(`/v1/routes?${rangeQuery()}`); routesErr = null; } catch (e) { routesErr = e; }
    if (tab === "queries") drawTab();
  }

  // ------------------------------------------------ drawing
  function drawScope() {
    const c = state.cluster, cs = copiesOf(c), all = state.fleet?.coordinators || [];
    // R2-workload-3: a coordinator the shards stopped hearing from is said apart.
    const coords = all.filter((x) => !x.gone), gone = all.filter((x) => x.gone);
    const goneText = gone.length ? ` ${gone.map((x) => x.name || x.url).join(", ")} stopped answering ${fmt.ago ? fmt.ago(Math.max(...gone.map((x) => x.last_seen_ms || 0))) : "recently"}: what went through ${gone.length === 1 ? "it" : "them"} is not here.` : "";
    const r = timeRange();
    const ns = (c?.shards || []).map((x) => x.replicas?.length || 0), lo = Math.min(...ns), hi = Math.max(...ns);
    const per = ns.length ? (lo === hi ? ` × ${fmt.plural(lo, "copy", "copies")}` : `, ${lo}-${hi} copies`) : "";
    scope.textContent = isCoord()
      ? `The fleet: ${coords.length > 1 ? `${fmt.plural(coords.length, "coordinator")}, ` : ""}${fmt.plural(c?.shards?.length || 0, "shard")}${per} · ${r.label}${against()}. ${coords.length > 1 ? `The charts are the queries through this coordinator (${coords.find((x) => x.this)?.name || location.host}); Home adds up ${coords.length === 2 ? "both" : "all " + coords.length}.` : state.fleet?.derived ? "The charts are the queries through this coordinator: this server does not list the others." : gone.length ? "The charts are the queries through this coordinator, the only one answering." : "Every query enters through this coordinator: the shards heard from no other in the last minute."}${goneText}`
      : `This server · ${r.label}${against()}.`;
  }
  /** What the "was" numbers are of, or why there are none. */
  function against() {
    return prev ? `, against ${baseWords()}` : base !== "before" && prevNone ? `; ${prevNone}` : "";
  }

  function draw() {
    const ps = visible();
    qpsChart();
    for (const c of charts) c.update(ps);
    const hasHist = ps.some((p) => p.latency_hist);
    // Its cells are as wide as the points' interval, which changes with the range.
    if (state.interval !== hmInterval) { const nh = heatmap({ bounds: BOUNDS, domain, height: 180, interval: state.interval }); hm.replaceWith(nh); hm = nh; hmInterval = state.interval; }
    hm.style.display = hasHist ? "" : "none"; hmNote.style.display = hasHist ? "none" : "";
    if (hasHist) hm.update(ps);
    drawScope();
    const f = figures(ps);
    drawKpis(f);
    drawFindings(f, ps);
    drawCopies();
    drawSlower();
    if (tab !== "queries") drawTab();
  }

  function drawBands() {
    const r = timeRange(), label = BANDS.find(([v]) => v === band)?.[1] || fmt.ms(band), slowMs = state.system?.limits?.slow_ms ?? 200;
    const head = h("div", { class: "row wl-bands-head" }, h("b", { class: "small" }, "Who filled the slow bands"), bandSeg, h("span", { class: "small muted" }, r.label));
    if (bands === null) { clear(bandBox, head, h("div", { class: "small muted" }, "This server keeps no log store: start it with --log-store to see the statements and copies of the slow queries.")); return; }
    if (bands === undefined) { clear(bandBox, head, h("div", { class: "small muted" }, "Reading the log store…")); return; }
    if (bands.err) { clear(bandBox, head, h("div", { class: "small muted" }, `The log store did not answer: ${bands.err.message}`)); return; }
    const n = bands.st.reduce((a, x) => a + x.n, 0);
    const byUrl = (node) => copiesOf().find((c) => c.node === node || c.url === node);
    const stT = table(cols([
      { key: "fingerprint", label: "statement", render: (x) => h("div", { class: "mono small wl-clamp2", style: { maxWidth: narrow() ? "170px" : "340px" }, title: x.fingerprint }, x.fingerprint || "–") },
      { key: "n", label: "queries", num: true, render: (x) => fmt.n(x.n) },
      { key: "slowest", label: "slowest", num: true, render: (x) => fmt.ms(x.slowest) },
    ]), { rows: bands.st, sortKey: "n", desc: true, compact: true, onRow: (x) => { if (x.fingerprint) location.hash = statementLink(x.fingerprint); }, empty: h("div", { class: "small muted", style: { padding: "8px 12px" } }, `No query took over ${label} in the range.`) });
    const cpT = bands.cp ? table(cols([
      { key: "node", label: "copy", render: (x) => { const c = byUrl(x.node); return h("span", { class: "badge mono", title: c ? copyTitle(c.url) : x.node }, c ? c.name : x.node); } },
      { key: "n", label: "shard parts", num: true, render: (x) => fmt.n(x.n) },
      { key: "slowest", label: "slowest", num: true, render: (x) => fmt.ms(x.slowest) },
    ]), { rows: bands.cp, sortKey: "n", desc: true, compact: true, onRow: (x) => { const c = byUrl(x.node); if (c) location.hash = copyLink(c.url); }, empty: h("div", { class: "small muted", style: { padding: "8px 12px" } }, `No shard took over ${label} for its part.`) }) : null;
    clear(bandBox, head,
      h("div", { class: "grid " + (cpT ? "g2" : ""), style: { gap: "10px" } },
        h("div", { class: "wl-nums" }, h("div", { class: "small dim", style: { margin: "2px 0 4px" } }, n ? `${fmt.plural(n, "query", "queries")} over ${label}, by statement` : "Statements"), stT),
        cpT ? h("div", { class: "wl-nums" }, h("div", { class: "small dim", style: { margin: "2px 0 4px" } }, "Their parts on the shards, by copy"), cpT) : null),
      h("div", { class: "small muted", style: { marginTop: "6px" } },
        band < slowMs ? `From the log store. It keeps every query over the slow threshold (${fmt.ms(slowMs)}) and a sample of faster ones, so these counts are low. ` : `From the log store, which keeps every query over the slow threshold (${fmt.ms(slowMs)}). `,
        h("a", { class: "wl-link", href: link("queries", { tab: "history", min: band }) }, "These queries in History ›")));
  }

  function drawKpis(f) {
    const sys = state.system, slowMs = sys?.limits?.slow_ms;
    const was = (v, fm) => (prev && v != null ? `was ${fm(v)}` : null);
    const sr = f.queries ? f.server / f.queries : null, psr = prev?.queries ? prev.server / prev.queries : null;
    const cs = copiesOf().filter((c) => c.cpu != null).sort((a, b) => b.cpu - a.cpu);
    const busiest = cs[0];
    // NEW-workload-5: the busiest and the median with the same digits.
    const cpuD = busiest && busiest.cpu < 0.1 ? 1 : 0;
    const coords = state.fleet?.coordinators || [];
    const apart = [f.client ? `client errors ${fmt.si(f.client / Math.max(1, f.queries) * 100, 1)}%` : null, f.deadlines ? `deadlines ${fmt.si(f.deadlines / Math.max(1, f.queries) * 100, 1)}%` : null].filter(Boolean);
    clear(kpis,
      kpi("Queries", f.qps != null ? [fmt.si(f.qps), h("small", {}, "/s")] : "–", prev?.qps ? `${pct(f.qps, prev.qps)} vs ${base === "before" ? "before" : BASES.find(([k]) => k === base)[1]}` : coords.length > 1 ? `${fmt.si(coords.reduce((a, c) => a + (c.qps || 0), 0))}/s on ${coords.length} coordinators now` : "mean over the range"),
      kpi("p99 latency", f.p99 != null ? unit(fmt.ms(f.p99)) : "–", [was(prev?.p99, fmt.ms), f.p99Now != null && Math.abs(f.p99Now - f.p99) > 0.2 * f.p99 ? `last 5 min ${fmt.ms(f.p99Now)}` : null].filter(Boolean).join(" · ") || "mean over the range", (prev?.p99 && f.p99Now > 2 * prev.p99 && f.p99Now > 20) ? "bad" : null, "The mean of each interval's p99 over the range; the last 5 minutes' median beside it when it differs"),
      kpi("Failed on the server", sr != null ? [fmt.si(sr * 100, 2), h("small", {}, "%")] : "–", psr != null ? `was ${fmt.si(psr * 100, 2)}%` : apart.length ? "apart: " + apart.join(", ") : f.splits ? "server errors and timeouts" : "this server does not split client errors", sr > 0.01 ? "bad" : null, "Server errors and server timeouts over the queries that ended. Client errors (400) and the clients' own deadlines are counted apart."),
      kpi(slowMs ? `Slow (over ${fmt.ms(slowMs)})` : "Slow", slowMs && f.slow != null ? [fmt.si(f.slow), h("small", {}, "/s")] : "–", slowMs ? was(prev?.slow, (v) => fmt.si(v) + "/s") : "not counted: --slow-ms is not set"),
      kpi("Refused (429)", f.refusedS != null ? [fmt.si(f.refusedS), h("small", {}, "/s")] : "–", was(prev?.refusedS, (v) => fmt.si(v) + "/s") || (f.refusedS ? (canContainNow() ? "no slot or memory, or a contain rule" : "no slot or memory free") : "none")),
      isCoord() && busiest
        ? kpi("Busiest copy, CPU", [(busiest.cpu * 100).toFixed(cpuD), h("small", {}, "%")], h("span", { title: `${copyTitle(busiest.url)}\nthe median copy: ${(median(cs.map((c) => c.cpu)) * 100).toFixed(cpuD)}%` }, `${copyName(busiest.url)}, median ${(median(cs.map((c) => c.cpu)) * 100).toFixed(cpuD)}%`), busiest.cpu > 0.85 ? "warn" : null, "Each copy's CPU over its last minute, as a share of its cores")
        : kpi("CPU", f.cpu != null ? [fmt.si(f.cpu, 1), h("small", {}, " cores")] : "–", state.system?.cores ? `of ${state.system.cores}` : null));
  }

  // ------------------------------------------------ findings
  function drawFindings(fAll, ps) {
    const out = [], read = [], missing = [], checked = [];
    const r = timeRange();
    // Failures over the range, or over its last 15 minutes when they are worse there (a range dilutes what just started).
    const t1 = ps.length ? ps[ps.length - 1].t : Date.now();
    const f15 = (t1 - (ps[0]?.t ?? t1)) > 20 * 60e3 ? figures(ps.filter((p) => p.t >= t1 - 15 * 60e3)) : null;
    const worse = (k) => f15?.queries && fAll.queries && f15[k] / f15.queries > 1.5 * (fAll[k] / fAll.queries) ? f15 : fAll;
    const winOf = (x) => x === f15 ? "the last 15 minutes" : "the range";
    // Over the time the points cover: a server that started within the range has fewer.
    const step = (state.series?.interval_ms || 2000) / 1000;
    const secsOf = (x) => x === f15 ? 15 * 60 : Math.max(1, ps.length ? (t1 - ps[0].t) / 1000 + step : ((r.to ?? Date.now()) - r.from) / 1000);
    const f = fAll;
    if (ps.length) read.push(`${isCoord() && (state.fleet?.coordinators || []).length > 1 ? "this coordinator's" : "the"} series (${fmt.plural(ps.length, "point")})`); else missing.push("the series");
    if (stmts) read.push(stmts.windowed ? "the statements of the range" : "the statements since the server started"); else missing.push("the statements");
    const cs = isCoord() ? copiesOf() : [];
    const withDetail = cs.filter((c) => c.has);
    if (isCoord()) { if (withDetail.length) read.push(`${fmt.n(withDetail.length)} of ${fmt.n(cs.length)} copies`); else missing.push("the copies"); }
    if (state.fleet && !state.fleet.derived) read.push("the fleet's health");
    const own = new Set();

    // Server failures.
    if (f.queries) {
      const w = worse("server"), sr = w.server / w.queries;
      checked.push(`server failures ${fmt.si(sr * 100, 2)}%`);
      if (sr > 0.01) {
        own.add("error_ratio");
        const top = (stmts?.statements || []).filter((x) => !isOwn(x)).map((x) => ({ x, n: split(x.outcomes, x.calls).server })).sort((a, b) => b.n - a.n)[0];
        const allS = (stmts?.statements || []).reduce((a, x) => a + split(x.outcomes, x.calls).server, 0) || 1;
        out.push({ level: "bad", id: "error_ratio", title: `${fmt.si(sr * 100, 1)}% of queries fail on the server${w === f15 ? " in the last 15 minutes" : ""}${prev?.queries ? ` (${fmt.si(prev.server / prev.queries * 100, 2)}% ${baseWords()})` : ""}`,
          body: [`${fmt.si(w.server / secsOf(w), 1)}/s server errors and timeouts`, w.timeouts ? ` (${fmt.si(w.timeouts / w.server * 100, 0)}% timeouts)` : "", top?.n ? [". ", fmt.pct(top.n / allS, 0), " are one statement: ", h("code", {}, trim(top.x.sql, 90))] : "", "."],
          rule: `server errors and timeouts over 1% of the queries in ${winOf(w)}`, links: [top?.n ? ["Open the statement", statementLink(top.x.fingerprint)] : null, ["Errors in Logs", link("logs", { q: "level>=error" })]] });
      }
    }
    // Client errors, which are not the server's.
    if (f.queries && f.client) {
      const w = worse("client"), cr = w.client / w.queries;
      checked.push(`client errors ${fmt.si(cr * 100, 1)}%`);
      if (cr > 0.01) {
        const tops = (stmts?.statements || []).map((x) => ({ x, n: split(x.outcomes, x.calls).client })).filter((y) => y.n).sort((a, b) => b.n - a.n);
        const all = tops.reduce((a, y) => a + y.n, 0) || 1, t = tops[0], k = t?.x.keys?.slice().sort((a, b) => b.calls - a.calls)[0];
        out.push({ level: "warn", id: "client_errors", title: `${fmt.si(cr * 100, 1)}% of queries are client errors (400)${w === f15 ? " in the last 15 minutes" : ""}`,
          body: [`${fmt.si(w.client / secsOf(w), 0)}/s requests the server cannot run. Not a server fault: they are counted apart from the failures.`, t ? [" ", fmt.pct(t.n / all, 0), " are one statement", k ? [" from key ", h("b", { class: "mono" }, k.key)] : "", ": ", h("code", {}, trim(t.x.sql, 80)), "."] : ""],
          rule: `client errors (400) over 1% of the queries in ${winOf(w)}`, links: [t ? ["Open the statement", statementLink(t.x.fingerprint)] : null, t && k && canContainNow() ? [`Refuse it from ${k.key}…`, () => containDialog({ fingerprint: t.x.fingerprint, sql: t.x.sql, key: k.key, keys: t.x.keys?.map((y) => ({ key: y.key, calls: y.calls })), action: "refuse" })] : null] });
      }
    }
    // Client deadlines, apart.
    if (f.queries && f.deadlines && f.deadlines / f.queries > 0.005) {
      const tops = (stmts?.statements || []).map((x) => ({ x, n: x.outcomes?.client_deadline || 0 })).filter((y) => y.n).sort((a, b) => b.n - a.n);
      const k = tops[0]?.x.keys?.slice().sort((a, b) => b.calls - a.calls)[0];
      out.push({ level: "info", id: "client_deadlines", title: `${fmt.si(f.deadlines / f.queries * 100, 1)}% of queries pass the client's own deadline`,
        body: [`The client asked for less time than the query takes (its timeout_ms). Counted apart from the server's failures.`, k ? [" Mostly key ", h("b", { class: "mono" }, k.key), "."] : ""],
        rule: "client deadlines over 0.5% of the queries", links: [tops[0] ? ["Open the statement", statementLink(tops[0].x.fingerprint)] : null] });
    }
    // p99: did it rise, and does a copy explain it?
    const mins = minutes(ps).filter((m) => m.p99 != null);
    const slowCopy = (() => {
      const ok = withDetail.filter((c) => c.p99 != null);
      if (ok.length < 3) return null;
      const med = median(ok.map((c) => c.p99)), w = ok.slice().sort((a, b) => b.p99 - a.p99)[0];
      return w.p99 > 5 * med && w.p99 > 30 ? { ...w, med } : null;
    })();
    let rise = null;
    if (mins.length >= 6) {
      const base = prev?.p99 ?? median(mins.slice(0, Math.floor(mins.length / 2)).map((m) => m.p99));
      const cur = median(mins.slice(-5).map((m) => m.p99));
      if (base && cur > 2 * base && cur > 20) {
        const mid = (base + cur) / 2;
        let i = mins.length - 1;
        while (i > 0 && mins[i - 1].p99 > mid) i--;
        rise = { base, cur, since: mins[i].t };
      }
      checked.push(rise ? `p99 rose` : `p99 ${fmt.ms(cur)}${base ? ` (${fmt.ms(base)} ${prev ? baseWords() : "in the range's first half"})` : ""}`);
    }
    if (slowCopy || rise) {
      own.add("p99");
      const grew = (stmts?.statements || []).filter((x) => !isOwn(x)).map((x) => ({ x, c: change(x) })).filter((y) => y.c?.p99X >= 2).sort((a, b) => (b.x.share || 0) - (a.x.share || 0))[0];
      // S4-3: what changed (a new statement, or one with more calls) before the one that got slower.
      const rr = timeRange(), cause = stmts?.windowed ? causesAndVictims((stmts.statements || []).filter((x) => !isOwn(x)), (rr.to ?? Date.now()) - rr.from).cause : null;
      const causeS = cause && cause.x !== grew?.x && (cause.x.share || 0) >= 0.05 ? cause : null;
      const sinceCopy = slowCopy ? copySince(slowCopy) : null;
      const body = [];
      let heavyS = null;
      if (slowCopy) {
        body.push(`On shard ${slowCopy.shard}, copy `, h("b", { class: "mono", title: copyTitle(slowCopy.url) }, copyName(slowCopy.url)), ` answers at p99 ${fmt.ms(slowCopy.p99)}${sinceCopy ? ` since ${fmt.time(sinceCopy, false)}` : ""}; the fleet's median copy at ${fmt.ms(slowCopy.med)}. A search waits for the slowest shard it asks.`);
        const why = [];
        const medSeg = median(withDetail.map((c) => c.segments));
        if (slowCopy.segments != null && medSeg && slowCopy.segments > 3 * medSeg) why.push(`${fmt.n(slowCopy.segments)} segments (median ${fmt.n(medSeg)})`);
        if (slowCopy.merge_failing) why.push("its merges fail");
        if (slowCopy.disk > 0.9) why.push(`disk ${fmt.pct(slowCopy.disk, 0)} used`);
        if (slowCopy.cpu > 0.85) why.push(`CPU ${fmt.pct(slowCopy.cpu, 0)}`);
        if (why.length) body.push(` It has ${why.join(", ")}.`);
      }
      if (causeS) body.push(" What changed: ", h("code", {}, trim(causeS.x.sql, 70)), causeS.c.isNew ? `, new since ${when(causeS.x.first_seen_ms)}` : `, ×${causeS.c.callsX.toFixed(1)} the calls`, `, now ${fmt.pct(causeS.x.share || 0, 0)} of query time${causeS.x.keys?.[0]?.key ? `, from key ${causeS.x.keys[0].key}` : ""}.`);
      if (grew) body.push(" Slower most: ", h("code", {}, trim(grew.x.sql, 70)), ` (p99 ${fmt.ms(grew.x.prev.p99_ms)} → ${fmt.ms(grew.x.p99_ms)}, ${fmt.pct(grew.x.share || 0, 0)} of query time${grew.c.callsX < 1.5 ? `, calls flat: likely a victim${causeS ? " of it" : ""}` : ""}).`);
      else {
        // With no window before to compare with: the statement that takes the most time, when it is slow itself.
        const heavy = (stmts?.statements || []).filter((x) => !isOwn(x) && x.p99_ms > (rise?.cur ?? 0) * 0.5).sort((a, b) => (b.share || 0) - (a.share || 0))[0];
        if (heavy && heavy.share > 0.3) { body.push(" It takes ", h("b", {}, fmt.pct(heavy.share, 0)), " of the query time: ", h("code", {}, trim(heavy.sql, 70)), ` (p99 ${fmt.ms(heavy.p99_ms)}${heavy.keys?.[0]?.key ? `, from key ${heavy.keys[0].key}` : ""}).`); heavyS = heavy; }
      }
      out.push({ level: rise ? "bad" : "warn", id: "p99",
        title: rise ? `p99 rose from ${fmt.ms(rise.base)} to ${fmt.ms(rise.cur)} at ${fmt.time(rise.since, false)}${slowCopy ? ", and one copy explains it" : ""}` : `Copy ${copyName(slowCopy.url)} is ${Math.round(slowCopy.p99 / slowCopy.med)}× slower than the median copy`,
        body: rise && !slowCopy ? [`The last 5 minutes' p99 is ${fmt.ms(rise.cur)}; no single copy explains it (the slowest copies are within 5× of the median).`, ...body] : body,
        rule: `${rise ? `the last 5 minutes' p99 over 2× ${prev ? baseWords() : "the range's first half"} and over 20 ms` : ""}${rise && slowCopy ? "; " : ""}${slowCopy ? "a copy's p99 over 5× the fleet's median copy (each copy's last minute)" : ""}`,
        links: [slowCopy ? [`Open ${copyName(slowCopy.url)}`, copyLink(slowCopy.url)] : null, slowCopy ? ["Its logs", link("logs", { node: copyName(slowCopy.url), level: "warn" })] : null, slowCopy ? [`CPU profile of ${copyName(slowCopy.url)}`, nodeLink(slowCopy.url)] : null, causeS ? ["Open what changed", statementLink(causeS.x.fingerprint)] : null, grew ? [causeS ? "Open the slower one" : "Open the statement", statementLink(grew.x.fingerprint)] : heavyS ? ["Open the statement", statementLink(heavyS.fingerprint)] : null] });
      if (heavyS) own.add("share:" + heavyS.fingerprint);
    } else if (withDetail.some((c) => c.p99 != null)) {
      const ok = withDetail.filter((c) => c.p99 != null), w = ok.slice().sort((a, b) => b.p99 - a.p99)[0];
      checked.push(`slowest copy ${fmt.ms(w.p99)} (median ${fmt.ms(median(ok.map((c) => c.p99)))})`);
    }
    // Disk, every copy.
    const disks = withDetail.filter((c) => c.disk != null);
    if (disks.length) {
      const max = disks.slice().sort((a, b) => b.disk - a.disk)[0], over = disks.filter((c) => c.disk >= 0.85);
      const chk = (state.fleet?.checks || []).find((c) => c.id === "disk");
      checked.push(`disk at most ${fmt.pct(max.disk, 0)}`);
      if (over.length) {
        own.add("disk");
        out.push({ level: max.disk >= 0.95 ? "bad" : "warn", id: "disk",
          title: over.length === disks.length && disks.length > 1 ? `Disk ${fmt.pct(median(disks.map((c) => c.disk)), 0)} used on every copy` : `Disk over 85% on ${fmt.plural(over.length, "copy", "copies")} of ${fmt.n(disks.length)}`,
          body: [`The fullest is `, h("b", { class: "mono", title: copyTitle(max.url) }, copyName(max.url)), ` at ${fmt.pct(max.disk, 0)}${max.merge_failing ? " (its merges fail: they need room)" : ""}. A copy whose disk fills stops taking writes.`, chk?.days_to_full != null ? ` At this rate the fullest is full in about ${chk.days_to_full < 2 ? fmt.plural(Math.max(1, Math.round(chk.days_to_full * 24)), "hour") : `${fmt.si(chk.days_to_full, 1)} days`}.` : "", over.length > 1 && disks.length > 1 && over.length === disks.length ? " Copies that share a volume count it once each." : ""],
          rule: "a copy's disk_used_ratio at 85% or more, now · 90% is the alert line", links: [["Copies by disk in Cluster", link("cluster", { lens: "disk" })], [`Open ${copyName(max.url)}`, copyLink(max.url)]] });
      }
    } else if (!isCoord() && ps.length) {
      const d = ps[ps.length - 1].disk_used_ratio;
      if (d != null) { checked.push(`disk ${fmt.pct(d, 0)}`); if (d >= 0.85) out.push({ level: d >= 0.95 ? "bad" : "warn", id: "disk", title: `Disk ${fmt.pct(d, 0)} used`, body: ["A server whose disk fills stops taking writes."], rule: "disk_used_ratio at 85% or more, now", links: [] }); }
    }
    // Memory, before the kernel kills the process: resident against its
    // limit, its heap against the queries' budget, and how fast it rises.
    for (const m of memoryFindings(ps, withDetail)) { out.push(m.finding); }
    checked.push(memChecked(ps, withDetail));
    // One statement takes most of the time.
    const topS = (stmts?.statements || []).filter((x) => !isOwn(x)).sort((a, b) => (b.share || 0) - (a.share || 0))[0];
    if (topS && topS.share > 0.4 && topS.calls > 10 && !own.has("share:" + topS.fingerprint)) out.push({ level: "info", id: "share", title: `One statement takes ${fmt.pct(topS.share, 0)} of query time`, body: [h("code", {}, trim(topS.sql, 110)), ` · ${fmt.si(topS.calls / Math.max(1, (stmts?.span_ms ?? ((r.to ?? Date.now()) - r.from)) / 1000))} calls/s · p99 ${fmt.ms(topS.p99_ms)}.`], rule: stmts?.windowed ? "statement statistics of the range" : "statement statistics since the server started", links: [["Open the statement", statementLink(topS.fingerprint)]] });
    // Refused.
    if (f.refusedS > 0.1) {
      const refusedOf = (x) => (x.outcomes?.refused || 0) + (x.outcomes?.rejected || 0);
      const topR = (stmts?.statements || []).filter((x) => refusedOf(x) > 0).sort((a, b) => refusedOf(b) - refusedOf(a))[0];
      // Refusals on purpose: a contain rule turned most of them away (its count is since it was made).
      const inRange = f.refusedS * secsOf(f);
      const byRule = (rules || []).filter((x) => x.refused > 0).sort((a, b) => b.refused - a.refused);
      const ruled = byRule.reduce((a, x) => a + x.refused, 0);
      if (byRule.length && ruled >= 0.5 * inRange) {
        const top = byRule[0];
        out.push({ level: "info", id: "refused", title: `${fmt.si(f.refusedS)}/s queries refused (429), most by a contain rule`,
          body: [`Rule ${top.id} ${top.action === "refuse" ? "refuses" : `lets ${top.concurrency} at once run of`} ${top.match?.fingerprint ? "a statement" : "everything"}${top.match?.key ? " from key " : ""}`, top.match?.key ? h("b", { class: "mono" }, top.match.key) : "",
            `: ${fmt.n(top.refused)} refused since ${when(top.created_ms)}, ${top.by ? "by " + top.by + ", " : ""}“${top.reason}”. It ends ${fmt.ago(top.expires_ms)}.`, byRule.length > 1 ? ` ${fmt.plural(byRule.length - 1, "other rule")} refused ${fmt.n(ruled - top.refused)}.` : ""],
          rule: "refused over 0.1/s over the range, and the contain rules' refusals", links: [["Contain rules in Queries", link("queries", { tab: "live" })], top.match?.fingerprint ? ["Open the statement", statementLink(top.match.fingerprint)] : null] });
      } else
      out.push({ level: "warn", id: "refused", title: `${fmt.si(f.refusedS)}/s queries refused (429)`,
        body: [`The server turned them away before running them: no query slot was free${canContainNow() ? ", the queries' memory was at its limit, or a contain rule refused them" : " or the queries' memory was at its limit"}. Clients see 429 and may retry.`,
          topR ? [" Refused most: ", h("code", {}, trim(topR.sql, 80)), ` (${fmt.n(refusedOf(topR))} in the range).`] : ""],
        rule: "refused over 0.1/s over the range",
        links: [topR ? ["Open the statement", statementLink(topR.fingerprint)] : null, canContainNow() ? ["Contain rules in Queries", link("queries", { tab: "live" })] : null].filter(Boolean) });
    }
    // Replication lag and copies down, from the fleet's checks.
    for (const c of state.fleet?.checks || []) {
      if (c.status === "ok" || own.has(c.id)) continue;
      if (c.id === "lag") out.push({ level: c.status === "critical" ? "bad" : "warn", id: "lag", title: `A replica is ${fmt.si(c.max_txn)} transactions behind${c.max_s != null ? ` (${fmt.si(c.max_s)} s)` : ""}`, body: [`On ${Array.isArray(c.where) ? c.where.join(", ") : c.where || "a copy"}. Reads it serves are that far behind its primary.`], rule: "the fleet's health check: lag", links: [["Copies by lag in Cluster", link("cluster", { lens: "lag" })]] });
      if (c.id === "copies" && c.down) out.push({ level: "bad", id: "copies", title: `${fmt.plural(c.down, "copy", "copies")} of ${fmt.n(c.total)} down`, body: [`${(c.where || []).join(", ")}. Its shard answers from the other copies.`], rule: "the fleet's health check: copies", links: [["Open Cluster", link("cluster")]] });
    }
    // The server's own problems that no finding above covers.
    const covers = { error_ratio: /error|fail|avail/i, p99: /p99|latency|slow/i, disk: /disk/i };
    for (const p of state.fleet?.problems || []) {
      if (own.has(p.id) || out.some((x) => x.id === p.id || (covers[x.id] && covers[x.id].test(`${p.id} ${p.title}`)))) continue;
      out.push(fleetFinding(p, topS, out));
    }
    const order = { bad: 0, warn: 1, info: 2 };
    out.sort((a, b) => order[a.level] - order[b.level]);

    const sub = `${fmt.plural(out.length, "finding")} · ${r.label}`;
    const readLine = h("div", { class: "small muted", style: { marginBottom: "10px" } }, `Read: ${read.join(", ") || "nothing"}.`, missing.length ? h("span", { class: "wl-bad" }, ` Could not read: ${missing.join(", ")}.`) : "", narrow() ? "" : " Each finding says which rule fired on which numbers.");
    let bodyEl;
    if (out.length) bodyEl = h("div", { class: "grid g2" }, out.map(findingEl));
    else if (missing.length || !ps.length) bodyEl = h("div", { class: "row nw", style: { gap: "10px" } }, icon("info"), h("span", {}, `Nothing found in what could be read, but ${missing.join(" and ") || "the series"} could not be read: this is not an all-clear.`));
    else bodyEl = h("div", { class: "row nw", style: { gap: "10px" } }, h("span", { style: { color: "var(--good)" } }, icon("okc")), h("span", {}, `Nothing needs attention over the ${r.label.replace(/^last /, "last ")}. Checked: ${checked.join(", ")}.`));
    clear(findBox, card({ title: "What needs attention", sub, body: [readLine, bodyEl] }));
  }
  /** One of the server's own problems as a finding. Long advice goes in the text, under a short link. */
  function fleetFinding(p, topS, out) {
    const said = `${p.title} ${p.impact || ""}`.toLowerCase();
    const where = (p.where || []).filter((w) => !said.includes(String(w).toLowerCase()));
    const next = p.next?.text || "";
    const VIEWS = { cluster: "Open Cluster", queries: "Open Queries", performance: "See the statements below", tables: "Open Tables and storage", alerts: "Open Alerts", logs: "Open Logs" };
    const short = next.length <= 48 ? next : VIEWS[p.next?.view] || "Open it";
    const body = [p.impact ? p.impact + ". " : "", p.why ? h("span", { class: "muted" }, p.why.replace(/^./, (c) => c.toUpperCase()) + ". ") : "", where.length ? `Where: ${where.join(", ")}. ` : "", next.length > 48 && p.next?.view !== "performance" ? `Next: ${next.replace(/\.$/, "")}.` : ""];
    const links = [p.next && p.next.view !== "performance" ? [short, link(p.next.view, p.next.params || {})] : null];
    // The fleet's p99 check with one statement taking most of the time: name it here, not in a finding of its own.
    if (p.id === "p99" && topS && topS.share > 0.3) {
      body.push(" ", h("b", {}, fmt.pct(topS.share, 0)), " of the query time is one statement: ", h("code", {}, trim(topS.sql, 80)), ` (p99 ${fmt.ms(topS.p99_ms)}${topS.keys?.[0]?.key ? `, mostly key ${topS.keys[0].key}` : ""}).`);
      links.push(["Open the statement", statementLink(topS.fingerprint)]);
      const i = out.findIndex((x) => x.id === "share");
      if (i >= 0) out.splice(i, 1);
    }
    return { level: p.severity === "critical" ? "bad" : "warn", id: p.id, title: p.title, body, rule: state.fleet.derived ? "worked out here from the cluster" : "the fleet's health", links };
  }
  /** This server's memory now and how fast it rose, from its points: {rss, heap, budget, limit, perMin, etaMin}. */
  function ownMemory(ps) {
    const pts = ps.filter((p) => p.rss_bytes != null);
    const last = pts[pts.length - 1];
    if (!last) return null;
    const limit = state.system?.memory_limit_bytes ?? null, budget = last.query_memory_limit_bytes ?? state.system?.limits?.query_memory_bytes ?? null;
    // The rise over the last 2 minutes (1 at least), while it still rises
    // over the last 30 s: a step that already ended is not a trend.
    const at = (ms) => pts.find((p) => p.t >= last.t - ms);
    const old = at(120e3), late = at(30e3);
    const mins = old ? (last.t - old.t) / 60e3 : 0;
    const still = late && limit ? last.rss_bytes - late.rss_bytes > 0.01 * limit : false;
    const perMin = mins >= 1 ? (last.rss_bytes - old.rss_bytes) / mins : null;
    const etaMin = still && perMin > 0 && limit ? (limit - last.rss_bytes) / perMin : null;
    return { rss: last.rss_bytes, heap: last.heap_bytes ?? null, peak: last.heap_peak_bytes ?? null, qmem: last.query_memory_bytes ?? null, budget, limit, perMin, etaMin };
  }
  /** One server's memory as a finding, or null: who, its figures, and links. */
  function memoryFinding(who, m, { url = null, here = false } = {}) {
    if (!m?.rss) return null;
    const ofLimit = m.limit ? m.rss / m.limit : null;
    const rising = m.etaMin != null && m.etaMin < 15 && ofLimit > 0.5;
    const near = ofLimit != null && ofLimit >= 0.85, high = ofLimit != null && ofLimit >= 0.7;
    const overBudget = m.heap != null && m.budget && m.heap > m.budget;
    if (!near && !rising && !high && !overBudget) return null;
    const level = near || rising ? "bad" : "warn";
    const title = near ? `${who} uses ${fmt.pct(ofLimit, 0)} of its memory limit`
      : rising ? `${who}'s memory rises: at this rate it reaches its limit in about ${fmt.n(Math.max(1, Math.round(m.etaMin)))} min`
      : high ? `${who} uses ${fmt.pct(ofLimit, 0)} of its memory limit`
      : `${who}'s heap is over what the queries may hold`;
    const body = [`Resident ${fmt.bytes(m.rss)}${m.limit ? ` of a ${fmt.bytes(m.limit)} limit (the cgroup's, or the machine's memory)` : ""}`,
      m.heap != null ? `, heap ${fmt.bytes(m.heap)}` : "", m.budget ? `; the queries' budget is ${fmt.bytes(m.budget)}${m.qmem != null ? ` and they hold ${fmt.bytes(m.qmem)} of it` : ""}` : "", ". ",
      m.etaMin != null ? `It rose ${fmt.bytes(m.perMin)} a minute over the last 2 minutes, and still rises. ` : "",
      overBudget ? "The heap holds more than the budget counts: a join's or a sort's build side is not counted against it, so the budget does not refuse the query that holds it. " : "",
      "The kernel kills a process that passes its limit (out of memory), and every query on it fails."];
    const links = [];
    // What runs here now: the statement most likely to hold it.
    const g = here ? (liveGroups || [])[0] : null;
    if (g) {
      body.push(" Running here now, the longest: ", g.count > 1 ? `${g.count} × ` : "", h("code", {}, trim(g.sql, 80)), `${g.key ? ` from key ${g.key}` : ""}, for ${fmt.ms(g.oldest_ms)}.`);
      links.push(["Open the statement", statementLink(g.fingerprint)], ["Running now in Queries", link("queries", { tab: "live" })]);
    }
    links.push([url ? `Memory profile of ${copyName(url)}` : "Memory profile", link("profile", { tab: "memory", node: url })]);
    if (url) links.push([`Open ${copyName(url)}`, copyLink(url)]);
    return { level, id: "memory", title, body,
      rule: `resident over 85% of the limit, or still rising to reach it within 15 min (the last 2 minutes' rate); a warning over 70%, or a heap over the queries' budget`, links };
  }
  function memoryFindings(ps, withDetail) {
    const out = [];
    const own = memoryFinding(isCoord() ? "This coordinator" : "This server", ownMemory(ps), { here: true });
    if (own) out.push({ finding: own });
    for (const c of withDetail) {
      const f = memoryFinding(`Copy ${copyName(c.url)}`, { rss: c.rss, heap: c.heap, budget: c.qbudget, limit: c.memLimit }, { url: c.url });
      if (f) out.push({ finding: f, c });
    }
    // At most three named; the rest in one line on the third.
    return out.slice(0, 3);
  }
  function memChecked(ps, withDetail) {
    const xs = [ownMemory(ps), ...withDetail.map((c) => ({ rss: c.rss, limit: c.memLimit }))].filter((m) => m?.rss && m.limit);
    if (!xs.length) return "memory (no limit known)";
    const top = xs.reduce((a, m) => m.rss / m.limit > a ? m.rss / m.limit : a, 0);
    return `memory at most ${fmt.pct(top, 0)} of a limit`;
  }
  /** When a copy's p99 went over 5× the median, from /v1/load. */
  function copySince(c) {
    const x = load?.copies?.find((y) => y.url === c.url);
    if (!x?.points?.length) return null;
    let since = null;
    for (const p of x.points) { if (p.p99_ms > 5 * c.med) { if (since == null) since = p.t; } else since = null; }
    return since;
  }
  function findingEl(x) {
    const tone = x.level === "bad" ? "var(--crit)" : x.level === "warn" ? "var(--warn)" : "var(--info, var(--s1))";
    return h("div", { class: "wl-find " + x.level },
      h("div", { class: "wl-find-ic", style: { color: tone } }, icon(x.level === "info" ? "info" : "warn")),
      h("div", { style: { minWidth: 0 } },
        h("div", { class: "wl-find-t" }, x.title),
        h("div", { class: "wl-find-b" }, x.body),
        x.rule ? h("div", { class: "small muted", style: { marginTop: "4px" } }, "Rule: " + x.rule) : null,
        h("div", { class: "row", style: { gap: "6px", marginTop: "8px" } }, (x.links || []).filter(Boolean).map(([t, href]) => typeof href === "function" ? h("button", { class: "btn sm", onclick: href }, t) : h("a", { class: "btn sm", href }, t)))));
  }

  // ------------------------------------------------ every copy by p99
  function drawCopies() {
    if (!isCoord()) { clear(copiesBox); return; }
    const all = copiesOf(), cs = all.filter((c) => c.p99 != null).sort((a, b) => b.p99 - a.p99);
    const silent = all.filter((c) => c.p99 == null);
    const silentEl = silent.length ? h("span", { class: "wl-bad" }, `${silent.map((c) => c.name).join(", ")} ${silent.some((c) => c.status === "down") ? "down" : "sent nothing"}: not in the chart · `) : "";
    if (!cs.length) { clear(copiesBox, card({ title: "Copies by p99", body: h("div", { class: "small muted" }, silent.length ? [silentEl, "no copy sent its figures."] : "No copy sent its figures yet.") })); return; }
    const med = median(cs.map((c) => c.p99));
    const W = 1000, H = 70, bw = Math.min(W / cs.length, 40);
    const max = Math.log10(Math.max(1.1, cs[0].p99 + 1)), y = (v) => H - 4 - (Math.log10(v + 1) / max) * (H - 8);
    const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: "none", style: { width: "100%", height: H + "px", display: "block", cursor: "pointer" }, role: "img", "aria-label": "every copy's p99, slowest first" });
    cs.forEach((c, i) => {
      const bad = c.p99 > 5 * med && c.p99 > 30, warn = c.p99 > 3 * med && c.p99 > 20;
      const r = s("rect", { x: i * bw, y: y(c.p99), width: Math.max(bad || warn ? 6 : 0.6, bw - (cs.length > 200 ? 0 : 1)), height: H - 4 - y(c.p99), fill: bad ? "var(--crit)" : warn ? "var(--warn)" : "var(--s1)", opacity: bad || warn ? 1 : 0.7 });
      r.append(s("title", {}, `${copyName(c.url)} · shard ${c.shard} · p99 ${fmt.ms(c.p99)}\n${c.url}`));
      r.onclick = () => { location.hash = copyLink(c.url); };
      svg.append(r);
    });
    const named = cs.slice(0, 3).map((c) => h("a", { class: "wl-link mono", href: copyLink(c.url), title: copyTitle(c.url) }, `${copyName(c.url)} ${fmt.ms(c.p99)}`));
    clear(copiesBox, card({ title: "Copies by p99", sub: "every copy of every shard, the slowest first · each copy's last minute · click one to open it", attrs: { style: { marginBottom: "14px" } }, body: [svg,
      h("div", { class: "row small muted", style: { justifyContent: "space-between", marginTop: "6px" } }, h("span", { class: "row nw", style: { gap: "10px" } }, named), h("span", {}, silentEl, `${fmt.plural(cs.length, "copy", "copies")} · median ${fmt.ms(med)} · log scale`))] }));
  }

  // ------------------------------------------------ what got slower
  function drawSlower() {
    if (!stmts) { clear(slowerBox, stmtsErr ? card({ title: "What got slower", body: h("div", { class: "small muted" }, "The statements could not be read: " + stmtsErr.message) }) : null); return; }
    const r = timeRange(), secs = Math.max(1, (stmts.span_ms ?? ((r.to ?? Date.now()) - r.from)) / 1000);
    const list = stmts.statements.filter((x) => !isOwn(x));
    if (!stmts.windowed) {
      const top = list.slice().sort((a, b) => (b.total_ms || 0) - (a.total_ms || 0)).slice(0, 6);
      clear(slowerBox, card({ title: "Where the time goes", sub: "statements by share of query time · since the server started: a newer server compares each with the window before", flush: true, body: table(cols([
        { key: "sql", label: "statement", render: (x) => h("a", { class: "mono small ellipsis wl-link", style: { display: "block", maxWidth: narrow() ? "210px" : "640px" }, href: statementLink(x.fingerprint), title: x.sql }, x.sql) },
        { key: "share", label: "share of time", num: true, render: (x) => fmt.pct(x.share || 0, 0) },
        { key: "calls", label: "calls", num: true, wide: true, render: (x) => fmt.n(x.calls) },
        { key: "p99_ms", label: "p99", num: true, render: (x) => fmt.ms(x.p99_ms) },
        { key: "fail", label: "failed", num: true, wide: true, sort: (x) => split(x.outcomes, x.calls).failed, render: (x) => outcomeBar(x.outcomes, 70) },
      ]), { rows: top, sortKey: "share", desc: true, compact: true, onRow: (x) => { location.hash = statementLink(x.fingerprint); } }) }));
      return;
    }
    const rows = list.map((x) => ({ x, c: change(x) })).filter((y) => y.c && y.c.p99X >= 1.3 && y.x.p99_ms - y.x.prev.p99_ms > 2).sort((a, b) => (b.x.p99_ms - b.x.prev.p99_ms) - (a.x.p99_ms - a.x.prev.p99_ms)).slice(0, 8);
    clear(slowerBox, card({ title: "What got slower", sub: `statements by change of p99 against the window before · ${r.label}`, flush: true, body: rows.length ? table(cols([
      { key: "sql", label: "statement", render: (y) => h("span", { class: "mono small ellipsis", style: { display: "block", maxWidth: narrow() ? "170px" : "560px" }, title: y.x.sql }, y.x.sql) },
      { key: "now", label: "p99 now", num: true, sort: (y) => y.x.p99_ms, render: (y) => h("b", { class: y.c.p99X >= 2 ? "wl-bad" : "" }, fmt.ms(y.x.p99_ms)) },
      { key: "before", label: "before", num: true, wide: true, sort: (y) => y.x.prev.p99_ms, render: (y) => h("span", { class: "muted" }, fmt.ms(y.x.prev.p99_ms)) },
      { key: "d", label: "change", num: true, sort: (y) => y.x.p99_ms - y.x.prev.p99_ms, render: (y) => "+" + fmt.ms(y.x.p99_ms - y.x.prev.p99_ms) },
      { key: "calls", label: "calls/s", num: true, wide: true, sort: (y) => y.x.calls, render: (y) => h("span", { title: `${fmt.n(y.x.calls)} calls; ${fmt.n(y.x.prev.calls)} the window before` }, fmt.si(y.x.calls / secs)) },
      { key: "fail", label: "failed", num: true, wide: true, sort: (y) => split(y.x.outcomes, y.x.calls).failed, render: (y) => outcomeBar(y.x.outcomes, 60) },
      { key: "verdict", label: "", sortable: false, wide: true, render: (y) => h("span", { class: "small muted" }, y.c.verdict || "") },
    ]), { rows, sortKey: "d", desc: true, compact: true, onRow: (y) => { location.hash = statementLink(y.x.fingerprint); } }) : h("div", { class: "small muted", style: { padding: "12px" } }, `No statement's p99 rose by a third or more against the window before (${fmt.plural(list.length, "statement")} compared).`) }));
  }

  // ------------------------------------------------ the tabs below
  function drawTabs() {
    clear(tabs, TABS.map(([k, v]) => h("button", { role: "tab", "aria-selected": String(k === tab), onclick: () => showTab(k) }, v)),
      narrow() ? null : [h("span", { class: "sep" }), h("span", { class: "small muted", style: { alignSelf: "center" } }, isCoord() ? "Writes and Resources are this coordinator's; Storage and Replication list every copy" : "This server")]);
  }
  const sub = [];
  function drawTab() {
    sub.forEach((c) => { const i = charts.indexOf(c); if (i >= 0) charts.splice(i, 1); });
    sub.length = 0;
    const SC = (title, sub_, opts) => { const c = timeChart({ height: 160, domain, onBrush: brush, ...opts }); sub.push(c); charts.push(c); c.update(visible()); return card({ title, sub: sub_, body: c }); };
    const who = isCoord() ? "this coordinator" : "this server";
    if (tab === "queries") { clear(tabBody, routesCard()); return; }
    if (tab === "writes") {
      clear(tabBody, h("div", { class: "grid g2" },
        SC("Write requests per second", who, { series: [{ name: "writes/s", get: (p) => p.writes_s }, { name: "failed", get: (p) => p.write_errors_s, color: "var(--crit)" }] }),
        SC("Rows written per second", who, { series: [{ name: "rows/s", get: (p) => p.rows_s, area: true, color: SERIES[2] }] }),
        SC("Write latency, p99", `${who} · from the request to the commit its acks wait for`, { fmt: fmt.ms, series: [{ name: "p99", get: (p) => p.write_p99_ms, color: SERIES[6], area: true }] }),
        SC("Write queue", `${who} · writes waiting for the writer thread`, { series: [{ name: "queued", get: (p) => p.write_queue, color: SERIES[3], area: true }] })));
      return;
    }
    if (tab === "resources") {
      const cores = state.system?.cores;
      const parts = [
        SC("CPU", `${who} · cores in use${cores ? `, of ${cores}` : ""}`, { series: [{ name: "cores", get: (p) => p.cpu, area: true }], threshold: cores ? { value: cores, label: "all cores" } : null }),
        SC("Memory", `${who} · resident, heap and what queries and merges hold${memLimits()}`, { fmt: fmt.bytes, series: [{ name: "resident", get: (p) => p.rss_bytes ?? p.mem_bytes, area: true, color: SERIES[6] }, { name: "heap", get: (p) => p.heap_bytes, color: SERIES[0] }, { name: "query memory", get: (p) => p.query_memory_bytes, color: SERIES[3] }, { name: "merges", get: (p) => p.merge_memory_bytes, color: SERIES[2] }], threshold: state.system?.memory_limit_bytes ? { value: state.system.memory_limit_bytes, label: "limit" } : null }),
        // A coordinator's queries take no slot of their own: "running" is every query it is answering.
        SC("Concurrency", `${who} · queries being answered and slots in use`, { series: [{ name: "queries", get: (p) => p.live ?? p.running }, { name: "slots in use", get: (p) => state.system?.limits?.max_concurrent && p.slots_free != null ? state.system.limits.max_concurrent - p.slots_free : null, color: SERIES[2] }], threshold: state.system?.limits?.max_concurrent ? { value: state.system.limits.max_concurrent, label: "slots" } : null }),
      ];
      if (isCoord()) parts.push(copyTable("Copies by CPU", "each copy's last minute, the busiest first", (c) => c.cpu, [
        { key: "cpu", label: "CPU", num: true, render: (c) => c.cpu != null ? h("span", { class: c.cpu > 0.85 ? "wl-bad" : "" }, fmt.pct(c.cpu, 0)) : "–" },
        { key: "qps", label: "q/s", num: true, render: (c) => fmt.si(c.qps) },
        { key: "p99", label: "p99", num: true, render: (c) => fmt.ms(c.p99) },
        { key: "rss", label: "memory", num: true, render: (c) => c.rss == null ? "–" : h("span", { class: c.memLimit && c.rss / c.memLimit >= 0.85 ? "wl-bad" : "", title: c.memLimit ? `${fmt.pct(c.rss / c.memLimit, 0)} of its ${fmt.bytes(c.memLimit)} limit${c.heap != null ? ` · heap ${fmt.bytes(c.heap)}` : ""}` : "" }, fmt.bytes(c.rss)) },
      ]));
      else parts.push(SC("Disk", "used of the database's file system", { fmt: (v) => fmt.pct(v, 0), max: 1, series: [{ name: "used", get: (p) => p.disk_used_ratio, area: true, color: SERIES[1] }], threshold: { value: 0.9, label: "alert" } }));
      clear(tabBody, h("div", { class: "grid g2" }, parts));
      return;
    }
    if (tab === "storage") {
      if (isCoord()) {
        const cs = copiesOf(), segMed = median(cs.map((c) => c.segments));
        clear(tabBody, copyTable("Copies by disk", "every copy, the fullest first · from each copy's latest figures", (c) => c.disk, [
          { key: "disk", label: "disk used", num: true, render: (c) => c.disk != null ? h("span", { class: c.disk >= 0.95 ? "wl-bad" : "" }, fmt.pct(c.disk, 0)) : "–" },
          { key: "segments", label: "segments", num: true, render: (c) => h("span", { class: segMed && c.segments > 3 * segMed ? "wl-bad" : "" }, c.segments ?? "–") },
          { key: "merges", label: "merges", sort: (c) => c.merge_failing ? 1 : 0, render: (c) => c.merge_failing ? h("span", { class: "pill bad" }, "failing") : c.flush_failing ? h("span", { class: "pill bad" }, "flushes fail") : h("span", { class: "muted" }, "ok") },
          { key: "docs", label: "rows", num: true, render: (c) => fmt.si(c.docs) },
          { key: "deleted", label: "deleted", num: true, render: (c) => c.deleted != null ? fmt.si(c.deleted) : "–" },
          { key: "file", label: "file", num: true, render: (c) => c.file != null ? fmt.bytes(c.file) : "–" },
        ], 24));
        return;
      }
      clear(tabBody, h("div", { class: "grid g2" },
        SC("Rows", null, { series: [{ name: "rows", get: (p) => p.docs, area: true }, { name: "deleted, not merged", get: (p) => p.deleted_docs, color: SERIES[7] }] }),
        SC("Bytes on disk", null, { fmt: fmt.bytes, series: [{ name: "database file", get: (p) => p.file_bytes, color: SERIES[0] }, { name: "write-ahead log", get: (p) => p.wal_bytes, color: SERIES[3] }, { name: "reusable", get: (p) => p.reusable_bytes, color: SERIES[2] }] }),
        SC("Segments", "and merges running", { series: [{ name: "segments", get: (p) => p.segments }, { name: "merges", get: (p) => p.merges_running, color: SERIES[2] }] }),
        SC("Disk", "used of the database's file system", { fmt: (v) => fmt.pct(v, 0), max: 1, series: [{ name: "used", get: (p) => p.disk_used_ratio, area: true, color: SERIES[1] }], threshold: { value: 0.9, label: "alert" } })));
      return;
    }
    // replication
    if (isCoord()) {
      clear(tabBody, copyTable("Replicas by lag", "transactions each replica has still to apply from its primary, the furthest behind first", (c) => c.behind, [
        { key: "behind", label: "behind", num: true, render: (c) => c.behind != null ? h("span", { class: c.behind > 10000 ? "wl-bad" : "" }, fmt.si(c.behind)) : "–" },
        { key: "role", label: "role", render: (c) => h("span", { class: "badge" }, c.role || "–") },
        { key: "status", label: "status", render: (c) => c.status === "ok" ? h("span", { class: "muted" }, "ok") : h("span", { class: "pill bad" }, c.status || "–") },
      ], 24, (c) => c.role !== "primary"));
      return;
    }
    clear(tabBody, h("div", { class: "grid g2" },
      SC("Replicas behind", "the most transactions a replica lacks", { series: [{ name: "behind", get: (p) => p.replica_behind_max, color: SERIES[3], area: true }] }),
      SC("Replicas in sync", null, { series: [{ name: "in sync", get: (p) => p.replicas_in_sync, color: SERIES[2] }, { name: "out of sync", get: (p) => p.replicas_out_of_sync, color: "var(--crit)" }] })));
  }
  /** The limits the Memory chart is read against, in words. */
  function memLimits() {
    const lim = state.system?.memory_limit_bytes, b = state.system?.limits?.query_memory_bytes;
    return [lim ? `limit ${fmt.bytes(lim)}` : null, b ? `queries' budget ${fmt.bytes(b)}` : null].filter(Boolean).map((x) => " · " + x).join("");
  }
  /** A table of copies sorted by a figure, the top N shown. */
  function copyTable(title, sub_, by, colsOf, n = 12, keep = () => true) {
    const cs = copiesOf().filter(keep).filter((c) => by(c) != null).sort((a, b) => by(b) - by(a));
    const shown = cs.slice(0, n);
    const acts = (c) => [h("a", { class: "btn sm", href: copyLink(c.url) }, "Open"), h("a", { class: "btn sm", href: nodeLink(c.url) }, "Profile")];
    // On a phone: the copy and the first figure; a tap opens the rest.
    const nar = narrow();
    const shownCols = nar ? colsOf.slice(0, 1) : colsOf;
    const onRow = nar ? (c) => sheet(`Copy ${copyName(c.url)}`, [["shard", c.shard], ...colsOf.map((x) => [x.label, x.render(c)]), ["address", h("span", { class: "mono small" }, c.url)]], acts(c)) : null;
    return card({ title, sub: sub_ + (nar ? " · tap one for the rest" : ""), flush: true, body: [table(cols([
      { key: "copy", label: "copy", sort: (c) => c.shard * 10 + c.j, render: (c) => h("a", { class: "mono wl-link", href: copyLink(c.url), title: copyTitle(c.url) }, copyName(c.url)) },
      { key: "shard", label: "shard", num: true },
      ...shownCols,
      { key: "act", label: "", sortable: false, wide: true, render: (c) => h("div", { class: "row nw", style: { gap: "4px" } }, acts(c)) },
    ]), { rows: shown, compact: true, onRow, empty: h("div", { class: "small muted" }, "No copy sent this figure.") }),
    cs.length > n ? h("div", { class: "small muted", style: { padding: "8px 12px" } }, `${fmt.plural(cs.length - n, "more copy", "more copies")}: `, h("a", { class: "wl-link", href: link("cluster") }, "all of them in Cluster")) : null] });
  }

  const OWN_ROUTE = /^(\/|\/ui(\/.*)?|\/favicon.*|\/v1\/(timeseries|health(\/.*)?|cluster|alerts(\/.*)?|system|whoami|events|load|routes|queries(\/.*)?|logs(\/.*)?|profile(\/.*)?))$/;
  function routesCard() {
    if (routesErr) return card({ title: "Kinds and routes", body: h("div", { class: "small muted" }, "Could not read the routes: " + routesErr.message) });
    if (!routes) return card({ title: "Kinds and routes", body: h("div", { class: "small muted" }, "Reading…") });
    const kinds = (routes.kinds || []).slice().sort((a, b) => b.queries - a.queries);
    const all = routes.http || [], ownR = all.filter((r) => OWN_ROUTE.test(r.route)), rs = showOwnRoutes ? all : all.filter((r) => !OWN_ROUTE.test(r.route));
    const maxK = Math.max(1e-9, ...kinds.map((k) => k.p99_ms || 0)), maxR = Math.max(1e-9, ...rs.map((k) => k.p99_ms || 0));
    const bar = (v, max, color) => h("div", { class: "row nw", style: { gap: "8px" } }, h("div", { class: "barbox" }, h("div", { class: "bar", style: { width: (max ? v / max * 100 : 0) + "%", background: color } })), h("span", { class: "num" }, fmt.ms(v)));
    const ownN = ownR.reduce((a, r) => a + r.requests, 0);
    const who = isCoord() ? "this coordinator" : "this server";
    const r_ = timeRange();
    const over = routes.from_ms == null ? "since it started" : routes.held_from_ms > routes.from_ms + 60e3 ? `${r_.label}, held from ${when(routes.held_from_ms)}` : r_.label;
    // The clients' own deadlines answer 504 too: say how many of the 5xx they are.
    const deadlines = kinds.reduce((a, k) => a + (k.outcomes?.client_deadline || 0), 0);
    // Red only for the server's own: the queries' 5xx beyond the clients' deadlines.
    const q5xx = all.filter((r) => /^\/v1\/(query|sql)$/.test(r.route)).reduce((a, r) => a + (r.server_errors || 0), 0);
    const theirs = (r) => /^\/v1\/(query|sql)$/.test(r.route) && q5xx <= deadlines;
    const wait = routes.admission_p99_ms;
    return h("div", { class: "grid g2" },
      card({ title: "Queries by plan kind", sub: `${who} · ${over} · click one for its queries in History`, flush: true, cls: "wl-nums", body: table(cols([
        { key: "kind", label: "kind", render: (k) => h("span", { class: "badge mono" }, k.kind) },
        { key: "queries", label: "queries", num: true, render: (k) => fmt.n(k.queries) },
        { key: "outcomes", label: "outcomes", wide: true, sort: (k) => split(k.outcomes, k.queries).failed / Math.max(1, k.queries), render: (k) => k.outcomes ? outcomeBar(k.outcomes, 80) : "–" },
        { key: "p50_ms", label: "p50", num: true, wide: true, render: (k) => fmt.ms(k.p50_ms) },
        { key: "p99_ms", label: "p99", title: "Estimated from latency buckets (1, 2, 5, 10 ms…)", render: (k) => narrow() ? fmt.ms(k.p99_ms) : bar(k.p99_ms, maxK, SERIES[0]) },
        { key: "hist", label: "latency", sortable: false, wide: true, render: (k) => k.hist ? sparkline(k.hist, { width: 100, height: 22, color: SERIES[0], cls: "" }) : "" },
      ]), { rows: kinds, sortKey: "queries", desc: true, compact: true, onRow: (k) => { location.hash = link("queries", { tab: "history", kind: k.kind }); }, empty: h("div", { class: "small muted" }, `No query ended ${routes.from_ms == null ? "since it started" : "in the range"}.`) }) }),
      card({ title: "HTTP routes", cls: "wl-nums", sub: `${who} · ${over}${wait != null ? ` · waiting for a slot: p99 ${wait <= 1 ? "under 1 ms" : fmt.ms(wait)}` : ""} · click one for its log lines`, flush: true, body: [table(cols([
        { key: "route", label: "route", render: (r) => h("span", { class: "mono small" }, r.route) },
        { key: "requests", label: "requests", num: true, render: (r) => fmt.n(r.requests) },
        { key: "client_errors", label: "4xx", num: true, wide: true, render: (r) => fmt.n(r.client_errors) },
        { key: "server_errors", label: "5xx", num: true, render: (r) => r.server_errors ? h("span", { class: theirs(r) ? "dim" : "wl-bad", title: theirs(r) ? "All of them are the clients' own deadlines (504): see the note below" : /^\/v1\/(query|sql)$/.test(r.route) && deadlines ? "504s include the clients' own deadlines: see the note below" : "" }, fmt.n(r.server_errors)) : "0" },
        { key: "p99_ms", label: "p99", title: "Estimated from latency buckets (1, 2, 5, 10 ms…)", render: (r) => narrow() ? fmt.ms(r.p99_ms) : bar(r.p99_ms, maxR, SERIES[0]) },
      ]), { rows: rs, sortKey: "requests", desc: true, compact: true, maxHeight: "360px", onRow: (r) => { location.hash = link("logs", { q: "route:" + r.route }); } }),
      deadlines ? h("div", { class: "small muted", style: { padding: "8px 12px 0" } }, `${fmt.n(deadlines)} of the queries passed the client's own deadline (timeout_ms) and answered 504: they are in the 5xx of /v1/query and /v1/sql, and are not the server's failures.`) : null,
      ownR.length ? h("div", { class: "small muted", style: { padding: "8px 12px" } }, showOwnRoutes ? "Showing the routes the command center reads. " : `Left out: ${fmt.n(ownN)} requests to ${fmt.plural(ownR.length, "route")} the command center reads (and health probes). `, h("a", { class: "wl-link", href: "#", onclick: (e) => { e.preventDefault(); showOwnRoutes = !showOwnRoutes; drawTab(); } }, showOwnRoutes ? "Leave them out" : "Show them")) : null] }));
  }

  drawTabs();
  drawTab();
  draw();
  drawBands();
  loadStmts(); loadPrev(); loadLoad(); loadBands(); if (tab === "queries") loadRoutes();
  const onRange = () => { kindsKey = ""; draw(); loadStmts(); loadPrev(); loadLoad(); bands = undefined; bandsKey = ""; drawBands(); loadBands(); if (tab === "queries") loadRoutes(); };
  const offs = [bus.on("points", draw), bus.on("range", onRange), bus.on("narrow", () => { drawTabs(); drawTab(); draw(); }), bus.on("theme", draw), bus.on("cluster", draw), bus.on("fleet", draw), bus.on("system", draw),
    poll(15000, loadStmts), poll(60000, loadLoad), poll(30000, loadBands), poll(15000, () => tab === "queries" ? loadRoutes() : null), poll(60000, loadPrev)];
  return () => offs.forEach((f) => f());
}

// ---------------------------------------------------------------- small parts

function kpi(label, val, sub, tone, title) {
  return h("div", { class: "card kpi" + (tone === "bad" ? " wl-kpi-bad" : ""), title: title || null },
    h("div", { class: "lbl" }, label), h("div", { class: "val" }, val),
    sub ? h("div", { class: "delta" + (tone === "bad" ? " up-bad" : ""), style: { maxWidth: "100%", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" } }, sub) : null);
}
const unit = (t) => { const m = /^([\d.]+) (\S+)$/.exec(t); return m ? [m[1], h("small", {}, m[2])] : t; };
const pct = (a, b) => { const x = (a - b) / Math.max(1e-9, b); return (x >= 0 ? "+" : "−") + fmt.pct(Math.abs(x), 0); };
const trim = (t, n) => (t || "").length > n ? t.slice(0, n - 1) + "…" : t || "";
