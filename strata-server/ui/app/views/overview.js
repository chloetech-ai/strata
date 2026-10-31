// Overview: is everything well, and if not where to look. A banner of the
// overall state, the headline numbers with their trend, traffic and
// latency, the cluster (or the storage of one server), the query mix,
// what is firing, what costs the most, and what is running now.

import { h, icon, clear, fmt, card, every, api, empty, bus, ApiError, tipOn } from "../core.js";
import { timeChart, sparkline, donut, gauge, SERIES, barList } from "../charts.js";
import { state, domain, visible, last, setZoom, isCoordinator } from "../data.js";
import { topoMini, storageFlow } from "./topology.js";

export default { id: "overview", title: "Overview", icon: "overview", group: "Monitor", keywords: "home dashboard health status kpi", render };

const avg = (ps, f) => { const v = ps.map(f).filter((x) => x != null && Number.isFinite(x)); return v.length ? v.reduce((a, b) => a + b, 0) / v.length : null; };

function render(page) {
  const banner = h("div", { class: "banner" });
  page.append(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Overview"), h("p", {}, "Health, traffic and saturation of this server and the copies it reaches."))),
    banner);

  // Headline numbers.
  const kpiDefs = [
    { key: "qps", label: "Queries", icon: "queries", get: (p) => p.qps, val: (v) => [fmt.si(v), h("small", {}, "/s")], color: SERIES[0], link: "#/performance" },
    { key: "p99", label: "p99 latency", icon: "clock", get: (p) => p.p99_ms, val: (v) => fmtMsParts(v), color: SERIES[6], link: "#/performance", bad: (v) => v > 250 },
    { key: "err", label: "Error ratio", icon: "warn", get: (p) => p.error_ratio, val: (v) => [fmt.pct(v, 2)], color: SERIES[7], link: "#/queries?tab=recent", bad: (v) => v > 0.01 },
    { key: "rows", label: "Rows written", icon: "import", get: (p) => p.rows_s, val: (v) => [fmt.si(v), h("small", {}, "/s")], color: SERIES[2], link: "#/performance" },
    { key: "docs", label: "Rows stored", icon: "db", get: (p) => p.docs ?? (p === last() ? state.health?.docs : null), val: (v) => [fmt.si(v)], color: SERIES[3], link: "#/system", sub: (p) => p.segments != null ? fmt.plural(p.segments, "segment") : null },
    { key: "cpu", label: "CPU", icon: "cpu", get: (p) => p.cpu, val: (v) => [fmt.si(v), h("small", {}, "cores")], color: SERIES[4], link: "#/system", sub: () => state.system ? `of ${state.system.cores}` : null },
  ];
  const kpis = kpiDefs.map((d) => {
    const val = h("div", { class: "val" }, "–"), delta = h("div", { class: "delta" }, " ");
    const spark = h("span");
    const c = h("div", { class: "card kpi", dataset: { link: d.link }, role: "link", tabIndex: 0, title: "Open " + d.link.slice(2) },
      h("div", { class: "lbl" }, icon(d.icon), d.label), val, delta, spark);
    c.onclick = () => (location.hash = d.link);
    c.onkeydown = (e) => { if (e.key === "Enter") location.hash = d.link; };
    return { d, c, val, delta, spark };
  });
  page.append(h("div", { class: "grid g6", style: { marginBottom: "14px" } }, kpis.map((k) => k.c)));

  // Traffic and latency, two charts on one time axis.
  const brush = (a, b) => setZoom(a == null ? null : [a, b]);
  const traffic = timeChart({ height: 200, label: "throughput", series: [
    { name: "queries/s", get: (p) => p.qps, area: true },
    { name: "writes/s", get: (p) => p.writes_s, color: SERIES[2] },
    { name: "HTTP requests/s", get: (p) => p.http_s, color: SERIES[3], dash: "4 3" },
  ], domain, onBrush: brush, markers: alertMarkers });
  const latency = timeChart({ height: 200, label: "latency", fmt: (v) => fmt.ms(v), series: [
    { name: "p50", get: (p) => p.p50_ms, color: SERIES[0] },
    { name: "p95", get: (p) => p.p95_ms, color: SERIES[3] },
    { name: "p99", get: (p) => p.p99_ms, color: SERIES[6] },
  ], band: { lo: (p) => p.p50_ms, hi: (p) => p.p99_ms, color: SERIES[0], name: "p50–p99" }, domain, onBrush: brush, markers: alertMarkers });
  page.append(h("div", { class: "grid g2", style: { marginBottom: "14px" } },
    card({ title: "Throughput", sub: "drag to zoom · double-click to reset", body: traffic }),
    card({ title: "Query latency", sub: "percentiles of the queries that ended in each 2 s", body: latency, tools: h("a", { class: "btn ghost sm", href: "#/performance" }, "Heatmap", icon("right")) })));

  // The cluster, or one server's storage; the query mix.
  const topo = h("div", { style: { height: "300px" } });
  const mix = h("div", { class: "row nw", style: { gap: "18px", alignItems: "center" } });
  const sat = h("div", { class: "row", style: { justifyContent: "space-around", gap: "6px" } });
  const topoCard = card({ title: "Topology", sub: " ", body: topo, tools: h("a", { class: "btn ghost sm", href: "#/topology" }, "Open", icon("right")) });
  page.append(h("div", { class: "grid g3", style: { marginBottom: "14px" } },
    h("div", { class: "span2", style: { display: "contents" } }, topoCard),
    h("div", { class: "stack" },
      card({ title: "Query mix", sub: "by plan kind, over the range", body: mix }),
      card({ title: "Saturation", sub: "now", body: sat }))));
  topoCard.classList.add("span2");

  // What is firing, what costs, what runs.
  const firing = h("div");
  const top = h("div");
  const running = h("div");
  page.append(h("div", { class: "grid g3" },
    card({ title: "Alerts", body: firing, tools: h("a", { class: "btn ghost sm", href: "#/alerts" }, "Rules", icon("right")) }),
    card({ title: "Where the time goes", sub: "statements by total time", body: top, tools: h("a", { class: "btn ghost sm", href: "#/queries?tab=statements" }, "All", icon("right")) }),
    card({ title: "Running now", body: running, tools: h("a", { class: "btn ghost sm", href: "#/queries" }, "Live", icon("right")) })));

  function alertMarkers() { return (state.alerts?.events || []).filter((e) => e.state === "firing").map((e) => ({ t: e.ts_ms, label: `${e.name}: firing` })); }

  function drawBanner() {
    const hl = state.health; if (!hl) return;
    const fire = state.alerts?.firing || [];
    const crit = fire.filter((a) => a.severity === "critical");
    const kind = !state.online || hl.status === "down" || hl.status === "failing" || crit.length ? "bad" : hl.status === "degraded" || fire.length ? "warn" : "ok";
    banner.className = "banner " + kind;
    const sys = state.system;
    const what = kind === "ok" ? "All systems normal" : !state.online ? "The server does not answer" : hl.status === "failing" ? "This server cannot write" : hl.status === "down" ? "Shards are down" : hl.status === "degraded" ? "Degraded: some copies are not answering" : `${fmt.plural(fire.length, "alert")} firing`;
    clear(banner,
      h("div", { style: { width: "36px", height: "36px", borderRadius: "10px", display: "grid", placeItems: "center", background: `var(--${kind === "ok" ? "good" : kind === "warn" ? "warn" : "crit"}-soft)`, color: `var(--${kind === "ok" ? "good" : kind === "warn" ? "warn" : "crit"})` } }, icon(kind === "ok" ? "okc" : "warn", "lg")),
      h("div", { class: "grow" },
        h("div", { class: "big" }, what),
        h("div", { class: "small dim" }, [
          hl.error ? hl.error : null,
          fire.length ? fire.slice(0, 3).map((a) => a.name).join(" · ") : null,
          `${fmt.si(hl.docs)} rows · ${fmt.plural(hl.segments, "segment")}`,
          isCoordinator() ? fmt.plural(hl.shards?.length || 0, "shard") : null,
          sys ? `up ${fmt.dur(sys.uptime_s)} · v${sys.version} · ${sys.cores} cores · ${sys.simd}` : null,
        ].filter(Boolean).join("  ·  "))),
      fire.length ? h("a", { class: "btn sm", href: "#/alerts" }, "See alerts") : null);
  }

  function drawKpis() {
    const ps = visible(); const p = last();
    for (const k of kpis) {
      const v = p ? k.d.get(p) : null;
      clear(k.val, v == null ? "–" : k.d.val(v));
      const a = avg(ps, k.d.get);
      const sub = k.d.sub?.(p || {});
      k.delta.className = "delta" + (k.d.bad && v != null && k.d.bad(v) ? " up-bad" : "");
      k.delta.textContent = sub || (a != null ? `avg ${k.d.key === "p99" ? fmt.ms(a) : k.d.key === "err" ? fmt.pct(a, 2) : fmt.si(a)}` : " ");
      k.delta.title = state.zoom ? "average over the zoomed range" : `average over the last ${state.range} min`;
      k.spark.replaceChildren(sparkline(ps.slice(-120).map(k.d.get), { color: k.d.color }));
    }
  }

  function drawMix() {
    const ps = visible(); const tot = {};
    for (const p of ps) for (const [k, v] of Object.entries(p.by_kind || {})) tot[k] = (tot[k] || 0) + v * (p.secs || 2);
    const items = Object.entries(tot).sort((a, b) => b[1] - a[1]);
    const top = items.slice(0, 5), rest = items.slice(5).reduce((a, b) => a + b[1], 0);
    const all = [...top.map(([name, value], i) => ({ name, value, color: SERIES[i] })), ...(rest ? [{ name: "other", value: rest, color: "var(--muted)" }] : [])];
    const n = all.reduce((a, b) => a + b.value, 0);
    clear(mix, donut(all, { size: 130, thick: 16, center: fmt.si(n), sub: "queries", fmtv: (v) => fmt.si(v) + " queries" }),
      all.length ? h("div", { class: "grow" }, barList(all, { fmtv: (v) => fmt.pct(v / (n || 1), 0) })) : h("div", { class: "muted small" }, "No queries in the range yet."));
  }

  function drawSat() {
    const p = last() || {}; const sys = state.system;
    const slots = sys?.limits?.max_concurrent && p.slots_free != null ? 1 - p.slots_free / sys.limits.max_concurrent : null;
    const items = [
      ["CPU", p.cpu != null && sys ? p.cpu / sys.cores : null, "of all cores"],
      ["Memory", p.rss_bytes != null && sys?.memory_limit_bytes ? p.rss_bytes / sys.memory_limit_bytes : null, p.rss_bytes != null ? fmt.bytes(p.rss_bytes) + " resident" : ""],
      ["Disk", p.disk_used_ratio ?? null, p.disk_free_bytes != null ? fmt.bytes(p.disk_free_bytes) + " free" : ""],
      ["Query slots", slots, p.slots_free != null ? `${p.slots_free} free` : ""],
    ];
    clear(sat, items.map(([name, v, sub]) => tipOn(h("div", { style: { textAlign: "center" }, tabIndex: 0 }, gauge(v, { size: 92, label: name })), () => [h("div", { class: "t" }, name), h("div", { class: "r" }, h("b", {}, v == null ? "not known" : fmt.pct(v)), h("span", { class: "n" }, sub))])));
  }

  function drawTopo() {
    if (isCoordinator() && state.cluster?.shards) {
      topoCard.querySelector(".hd h3").textContent = "Topology";
      topoCard.querySelector(".sub").textContent = `${fmt.plural(state.cluster.shards.length, "shard")} · ${fmt.plural(state.cluster.shards.reduce((a, s) => a + s.replicas.length, 0), "copy", "copies")}`;
      topoMini(topo, state.cluster);
    } else if (state.health && !isCoordinator()) {
      topoCard.querySelector(".hd h3").textContent = "Storage";
      topoCard.querySelector(".sub").textContent = state.health?.replication ? "writes, the log, segments, and the replicas" : "how writes reach segments, and queries read them";
      storageFlow(topo, last() || {}, state.health, state.system);
    }
  }

  function drawFiring() {
    const a = state.alerts;
    if (!a) { clear(firing, empty("alerts", "No alert rules")); return; }
    const rules = a.rules || [];
    const hot = rules.filter((r) => r.status?.state === "firing" || r.status?.state === "pending");
    const ok = rules.filter((r) => r.status?.state === "ok").length;
    clear(firing,
      hot.length ? h("div", { class: "stack", style: { gap: "8px" } }, hot.map((r) => h("a", { href: "#/alerts?rule=" + encodeURIComponent(r.id), class: "row nw", style: { color: "inherit", textDecoration: "none", gap: "10px" } },
        h("span", { class: "dot pulse " + r.status.state }), h("div", { class: "grow" }, h("div", { style: { fontWeight: 600 } }, r.name), h("div", { class: "small muted" }, `${r.metric} ${r.op} ${r.threshold} · now ${r.status.value == null ? "–" : fmt.si(r.status.value)} · since ${fmt.ago(r.status.since_ms)}`)),
        h("span", { class: "pill " + (r.status.state === "firing" ? r.severity === "critical" ? "critical" : "warning" : "pending") }, r.status.state))))
        : h("div", { class: "row nw", style: { gap: "12px", padding: "6px 0" } }, h("span", { style: { color: "var(--good)" } }, icon("okc", "lg")), h("div", {}, h("div", { style: { fontWeight: 600 } }, "Nothing firing"), h("div", { class: "small muted" }, `${ok} of ${rules.length} rules ok`))),
      (a.events || []).length ? h("div", { class: "small muted", style: { marginTop: "12px", borderTop: "1px solid var(--line)", paddingTop: "8px" } }, "Latest: ", a.events.slice(0, 1).map((e) => `${e.name} ${e.state} ${fmt.ago(e.ts_ms)}`)) : null);
  }

  async function loadTop() {
    try {
      const j = await api("/v1/queries/stats?limit=5");
      const st = j.statements || [];
      clear(top, st.length ? h("div", { class: "stack", style: { gap: "10px" } }, st.map((x, i) => h("a", { href: "#/queries?tab=statements", style: { color: "inherit", textDecoration: "none" } },
        h("div", { class: "mono small ellipsis", title: x.fingerprint }, x.fingerprint),
        h("div", { class: "row nw small", style: { marginTop: "3px" } },
          h("div", { class: "meter grow" }, h("i", { style: { width: (x.share * 100).toFixed(1) + "%", background: SERIES[i] } })),
          h("span", { class: "dim num nowrap" }, `${fmt.pct(x.share, 0)} · ${fmt.n(x.calls)} calls · ${fmt.ms(x.mean_ms)} mean`)))))
        : empty("queries", "No statements yet", "Run a query and it shows here, grouped by shape."));
    } catch (e) { clear(top, e instanceof ApiError && e.status === 403 ? empty("lock", "Needs an admin key", "Statements show other clients' SQL.") : h("div", { class: "error" }, e.message)); }
  }

  async function loadRunning() {
    try {
      const j = await api("/v1/queries");
      const qs = j.queries || [];
      clear(running,
        h("div", { class: "row", style: { gap: "16px", marginBottom: "10px" } },
          h("div", {}, h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "val" }, fmt.n(qs.length))), h("div", { class: "small muted" }, "queries running")),
          state.system ? h("div", {}, h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "val" }, fmt.n(state.system.limits.max_concurrent))), h("div", { class: "small muted" }, "slots in all")) : null),
        qs.length ? h("div", { class: "stack", style: { gap: "6px" } }, qs.slice(0, 5).map((q) => h("div", { class: "row nw small" }, h("span", { class: "dot ok pulse" }), h("span", { class: "mono ellipsis grow", title: q.sql }, q.sql), h("b", { class: "num" }, fmt.ms(q.elapsed_ms)))))
          : h("div", { class: "small muted" }, "Nothing running. A query that runs long shows here with a button to cancel it."));
    } catch (e) { clear(running, e instanceof ApiError && e.status === 403 ? empty("lock", "Needs an admin key") : h("div", { class: "error" }, e.message)); }
  }

  function drawPoints() {
    const ps = visible();
    traffic.update(ps); latency.update(ps);
    drawKpis(); drawMix(); drawSat();
    if (!isCoordinator()) drawTopo();
  }
  drawBanner(); drawPoints(); drawTopo(); drawFiring();
  const offs = [
    bus.on("points", drawPoints), bus.on("range", drawPoints), bus.on("health", drawBanner), bus.on("online", drawBanner),
    bus.on("alerts", () => { drawFiring(); drawBanner(); }), bus.on("cluster", drawTopo), bus.on("health", () => { if (!topo.firstChild) drawTopo(); }), bus.on("system", () => { drawBanner(); drawSat(); }),
    bus.on("theme", drawPoints),
    every(5000, loadTop), every(2000, loadRunning),
  ];
  return () => offs.forEach((f) => f());
}

function fmtMsParts(v) {
  const t = fmt.ms(v); const i = t.lastIndexOf(" ");
  return i < 0 ? [t] : [t.slice(0, i), h("small", {}, t.slice(i + 1))];
}
