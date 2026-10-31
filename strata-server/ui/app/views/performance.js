// Performance: every rate and gauge of the series on one time axis
// (brush any chart to zoom them all), the latency heatmap, requests by
// route and queries by kind, and a tuning advisor that reads the numbers
// and says what to change.

import { h, icon, clear, fmt, card, every, api, bus, store } from "../core.js";
import { timeChart, heatmap, SERIES, sparkline } from "../charts.js";
import { state, domain, visible, setZoom } from "../data.js";

export default { id: "performance", title: "Performance", icon: "perf", group: "Monitor", keywords: "latency throughput heatmap tuning advisor cpu memory disk routes kinds percentiles", render };

const BOUNDS = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0];

const SECTIONS = {
  queries: "Queries",
  writes: "Writes",
  resources: "Resources",
  storage: "Storage",
  replication: "Replication",
};

function render(page) {
  const tab = store.get("perf.tab", "queries");
  const tabs = h("div", { class: "tabs", role: "tablist" });
  const body = h("div");
  page.append(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Performance"), h("p", {}, "Brush across any chart to zoom them all; double-click to reset. Click a legend entry to hide a series."))),
    h("div", { id: "advisor" }), tabs, body);
  const advisorHost = page.querySelector("#advisor");
  const brush = (a, b) => setZoom(a == null ? null : [a, b]);
  let charts = [];

  const C = (title, sub, opts, span) => {
    const c = timeChart({ height: opts.height || 190, domain, onBrush: brush, ...opts });
    charts.push(c);
    return card({ title, sub, body: c, cls: span ? "spanall" : "" });
  };
  const kinds = () => { const k = new Set(); for (const p of state.points) for (const x of Object.keys(p.by_kind || {})) k.add(x); return [...k].sort(); };

  function build(t) {
    charts = [];
    clear(tabs, Object.entries(SECTIONS).map(([k, v]) => h("button", { role: "tab", "aria-selected": String(k === t), onclick: () => { store.set("perf.tab", k); build(k); } }, v)));
    let grid;
    if (t === "queries") {
      const ks = kinds();
      const hm = heatmap({ bounds: BOUNDS, domain, height: 220, interval: state.interval });
      charts.push(hm);
      grid = [
        C("Queries per second, by kind", "stacked", { stack: true, series: (ks.length ? ks : ["queries"]).map((k, i) => ({ name: k, get: (p) => ks.length ? p.by_kind?.[k] || 0 : p.qps, color: SERIES[i % 8] })) }),
        C("Latency percentiles", "of the queries that ended in each interval", { fmt: fmt.ms, series: [{ name: "p50", get: (p) => p.p50_ms }, { name: "p95", get: (p) => p.p95_ms, color: SERIES[3] }, { name: "p99", get: (p) => p.p99_ms, color: SERIES[6] }], band: { lo: (p) => p.p50_ms, hi: (p) => p.p99_ms, name: "p50–p99" } }),
        card({ title: "Latency heatmap", sub: "every query by how long it took · darker is more", body: hm, cls: "spanall" }),
        C("Failures per second", "errors, refused for a slot (429), partial answers, HTTP 5xx", { series: [{ name: "errors", get: (p) => p.errors_s, color: SERIES[7] }, { name: "refused (429)", get: (p) => p.rejected_s, color: SERIES[3] }, { name: "partial", get: (p) => p.partial_s, color: SERIES[4] }, { name: "HTTP 5xx", get: (p) => p.http_5xx_s, color: SERIES[1] }] }),
        C("Concurrency", "queries running, slots in use, pool threads waited for", { series: [{ name: "running", get: (p) => p.running }, { name: "slots in use", get: (p) => state.system && p.slots_free != null ? state.system.limits.max_concurrent - p.slots_free : null, color: SERIES[2] }, { name: "pool queued", get: (p) => p.pool_queued, color: SERIES[3] }], threshold: state.system ? { value: state.system.limits.max_concurrent, label: "max concurrent" } : null }),
        C("Slow queries per second", state.system?.limits?.slow_ms ? `over ${state.system.limits.slow_ms} ms` : "set --slow-ms to count them", { series: [{ name: "slow", get: (p) => p.slow_s, color: SERIES[1], area: true }] }),
        C("HTTP requests per second", "by status class", { stack: true, series: [{ name: "2xx and 3xx", get: (p) => Math.max(0, (p.http_s || 0) - (p.http_4xx_s || 0) - (p.http_5xx_s || 0)), color: SERIES[0] }, { name: "4xx", get: (p) => p.http_4xx_s, color: SERIES[3] }, { name: "5xx", get: (p) => p.http_5xx_s, color: SERIES[7] }] }),
      ];
    } else if (t === "writes") {
      grid = [
        C("Rows written per second", null, { series: [{ name: "rows/s", get: (p) => p.rows_s, area: true, color: SERIES[2] }] }),
        C("Write requests per second", null, { series: [{ name: "writes/s", get: (p) => p.writes_s }, { name: "failed", get: (p) => p.write_errors_s, color: SERIES[7] }] }),
        C("Write latency, p99", "from the request to the commit its acks wait for", { fmt: fmt.ms, series: [{ name: "p99", get: (p) => p.write_p99_ms, color: SERIES[6], area: true }] }),
        C("Write queue", "writes waiting for the writer thread", { series: [{ name: "queued", get: (p) => p.write_queue, color: SERIES[3], area: true }], threshold: state.system?.limits?.max_queued_writes ? { value: state.system.limits.max_queued_writes, label: "max" } : null }),
        C("Memtable", "rows written but not flushed", { series: [{ name: "rows", get: (p) => p.memtable_docs, color: SERIES[4], area: true }] }),
        C("Leases", "rows reserved by a client's transaction", { series: [{ name: "leases", get: (p) => p.leases, color: SERIES[5] }] }),
      ];
    } else if (t === "resources") {
      const cores = state.system?.cores;
      grid = [
        C("CPU", cores ? `cores in use, of ${cores}` : "cores in use", { series: [{ name: "cores", get: (p) => p.cpu, area: true }], threshold: cores ? { value: cores, label: "all cores" } : null }),
        C("Memory", "resident set", { fmt: fmt.bytes, series: [{ name: "resident", get: (p) => p.rss_bytes, area: true, color: SERIES[6] }, { name: "query memory", get: (p) => p.query_memory_bytes, color: SERIES[3] }, { name: "merges", get: (p) => p.merge_memory_bytes, color: SERIES[2] }] }),
        C("Disk", "used of the database's file system", { fmt: (v) => fmt.pct(v, 0), max: 1, series: [{ name: "used", get: (p) => p.disk_used_ratio, area: true, color: SERIES[1] }], threshold: { value: 0.9, label: "alert" } }),
        C("Query memory", "of the budget the running queries share", { fmt: (v) => fmt.pct(v, 0), max: 1, series: [{ name: "used", get: (p) => p.query_memory_ratio, area: true, color: SERIES[3] }] }),
      ];
    } else if (t === "storage") {
      grid = [
        C("Rows", null, { series: [{ name: "rows", get: (p) => p.docs, area: true }, { name: "deleted, not merged", get: (p) => p.deleted_docs, color: SERIES[7] }] }),
        C("Bytes on disk", null, { fmt: fmt.bytes, series: [{ name: "database file", get: (p) => p.file_bytes, color: SERIES[0] }, { name: "write-ahead log", get: (p) => p.wal_bytes, color: SERIES[3] }, { name: "reusable", get: (p) => p.reusable_bytes, color: SERIES[2] }] }),
        C("Segments", "and merges running", { series: [{ name: "segments", get: (p) => p.segments }, { name: "merges", get: (p) => p.merges_running, color: SERIES[2] }] }),
        C("Failing", "1 when the server, its flushes or merges fail", { max: 1, series: [{ name: "server", get: (p) => p.failing, color: SERIES[7] }, { name: "flushes", get: (p) => p.flush_failing, color: SERIES[1] }, { name: "merges", get: (p) => p.merge_failing, color: SERIES[3] }] }),
      ];
    } else {
      grid = [
        C("Replicas behind", "the most transactions a replica lacks", { series: [{ name: "behind", get: (p) => p.replica_behind_max, color: SERIES[3], area: true }] }),
        C("Replicas in sync", null, { series: [{ name: "in sync", get: (p) => p.replicas_in_sync, color: SERIES[2] }, { name: "out of sync", get: (p) => p.replicas_out_of_sync, color: SERIES[7] }, { name: "replicas", get: (p) => p.replicas, color: SERIES[0], dash: "4 3" }] }),
        C("Sequence number", "the latest transaction", { series: [{ name: "sequence", get: (p) => p.seq_no }] }),
        C("Serves reads", "a replica: 1 when it answers reads", { max: 1, series: [{ name: "serves reads", get: (p) => p.serves_reads, color: SERIES[2] }] }),
      ];
    }
    clear(body, h("div", { class: "grid g2" }, grid), h("div", { id: "routes", style: { marginTop: "14px" } }));
    draw();
    loadRoutes();
  }

  function draw() { const ps = visible(); for (const c of charts) c.update(ps); advisor(); }

  async function loadRoutes() {
    const host = body.querySelector("#routes"); if (!host) return;
    try {
      const j = await api("/v1/routes");
      const bar = (v, max, color) => h("div", { class: "row nw", style: { gap: "8px" } }, h("div", { class: "barbox" }, h("div", { class: "bar", style: { width: (max ? v / max * 100 : 0) + "%", background: color } })), h("span", { class: "num" }, fmt.ms(v)));
      const maxK = Math.max(1e-9, ...j.kinds.map((k) => k.p99_ms || 0));
      const maxR = Math.max(1e-9, ...j.http.map((k) => k.p99_ms || 0));
      clear(host, h("div", { class: "grid g2" },
        card({ title: "Queries by plan kind", sub: "since the server started", flush: true, body: h("div", { class: "tbl-wrap" }, h("table", { class: "t" },
          h("thead", {}, h("tr", {}, ["kind", "queries", "mean", "p50", "p99", "latency"].map((x, i) => h("th", { class: i ? "num" : "" }, x)))),
          h("tbody", {}, j.kinds.sort((a, b) => b.queries - a.queries).map((k, i) => h("tr", {}, h("td", {}, h("span", { class: "badge mono" }, k.kind)), h("td", { class: "num" }, fmt.n(k.queries)), h("td", { class: "num" }, fmt.ms(k.mean_ms)), h("td", { class: "num" }, fmt.ms(k.p50_ms)), h("td", {}, bar(k.p99_ms, maxK, SERIES[i % 8])),
            h("td", {}, sparkline(k.hist, { width: 110, height: 24, color: SERIES[i % 8], cls: "" }))))))) }),
        card({ title: "HTTP routes", sub: j.admission_p99_ms != null ? `waiting for a slot: p99 ${fmt.ms(j.admission_p99_ms)}` : null, flush: true, body: h("div", { class: "tbl-wrap", style: { maxHeight: "360px" } }, h("table", { class: "t" },
          h("thead", {}, h("tr", {}, ["route", "requests", "4xx", "5xx", "mean", "p99"].map((x, i) => h("th", { class: i ? "num" : "" }, x)))),
          h("tbody", {}, j.http.sort((a, b) => b.requests - a.requests).map((r) => h("tr", {}, h("td", { class: "mono small" }, r.route), h("td", { class: "num" }, fmt.n(r.requests)), h("td", { class: "num" }, fmt.n(r.client_errors)), h("td", { class: "num", style: { color: r.server_errors ? "var(--crit-ink)" : null } }, fmt.n(r.server_errors)), h("td", { class: "num" }, fmt.ms(r.mean_ms)), h("td", {}, bar(r.p99_ms, maxR, SERIES[0])))))))}),
        j.grpc?.length ? card({ title: "gRPC calls", flush: true, cls: "spanall", body: h("div", { class: "tbl-wrap" }, h("table", { class: "t" },
          h("thead", {}, h("tr", {}, ["method", "calls", "failed", "mean", "p99"].map((x, i) => h("th", { class: i ? "num" : "" }, x)))),
          h("tbody", {}, j.grpc.map((r) => h("tr", {}, h("td", { class: "mono small" }, r.method), h("td", { class: "num" }, fmt.n(r.calls)), h("td", { class: "num" }, fmt.n(r.failed)), h("td", { class: "num" }, fmt.ms(r.mean_ms)), h("td", { class: "num" }, fmt.ms(r.p99_ms))))))) }) : null));
    } catch (e) { clear(host, h("div", { class: "error" }, e.message)); }
  }

  let stmts = null;
  async function loadStmts() { try { stmts = (await api("/v1/queries/stats?limit=20")).statements; } catch { stmts = null; } advisor(); }

  function advisor() {
    const tips = advise(visible(), state.system, stmts);
    clear(advisorHost, card({ title: "Tuning advisor", sub: tips.length ? `${fmt.plural(tips.length, "suggestion")} from the range shown` : "reads the numbers below", attrs: { style: { marginBottom: "14px" } },
      body: tips.length ? h("div", { class: "grid g2" }, tips.map((t) => h("div", { class: "advice " + t.level }, h("div", { class: "ic" }, icon(t.icon || (t.level === "bad" ? "warn" : t.level === "ok" ? "okc" : "wrench"))), h("div", {}, h("h4", {}, t.title), h("p", {}, t.body)))))
        : h("div", { class: "row nw", style: { gap: "10px" } }, h("span", { style: { color: "var(--good)" } }, icon("okc", "lg")), h("span", { class: "dim" }, "Nothing to change: latency, errors, memory, disk and the queues look healthy over this range.")) }));
  }

  build(tab);
  const offs = [bus.on("points", draw), bus.on("range", draw), bus.on("theme", draw), bus.on("system", () => build(store.get("perf.tab", "queries"))), every(10000, loadRoutes, false), every(10000, loadStmts)];
  return () => offs.forEach((f) => f());
}

const code = (t) => h("code", {}, t);

/** Suggestions from a range of points, the system and the costliest statements. */
export function advise(ps, sys, stmts) {
  const out = [];
  if (!ps.length || !sys) return out;
  const avg = (f) => { const v = ps.map(f).filter((x) => x != null && Number.isFinite(x)); return v.length ? v.reduce((a, b) => a + b, 0) / v.length : null; };
  const max = (f) => { const v = ps.map(f).filter((x) => x != null && Number.isFinite(x)); return v.length ? Math.max(...v) : null; };
  const p = ps[ps.length - 1];
  const L = sys.limits || {};
  const rejected = avg((x) => x.rejected_s);
  const busy = L.max_concurrent && p.slots_free != null ? max((x) => L.max_concurrent - (x.slots_free ?? L.max_concurrent)) : null;
  const cpu = avg((x) => x.cpu);
  if (rejected > 0.01) {
    if (cpu != null && cpu < sys.cores * 0.6) out.push({ level: "warn", icon: "gauge", title: "Queries are refused while CPU is idle", body: ["About ", fmt.rate(rejected), " answer 429 for want of a slot, but only ", fmt.si(cpu), " of ", sys.cores, " cores are busy. Raise ", code("--max-concurrent"), ` (now ${L.max_concurrent}).`] });
    else out.push({ level: "bad", icon: "gauge", title: "The server is saturated", body: ["About ", fmt.rate(rejected), " are refused (429) and the CPU is busy. Add shards or replicas that serve reads, or find the costliest statements below."] });
  } else if (busy != null && busy >= L.max_concurrent) out.push({ level: "warn", icon: "gauge", title: "Every query slot was in use", body: [`All ${L.max_concurrent} slots were taken at some point; the next query waits for one. If the CPU has room, raise `, code("--max-concurrent"), "."] });
  const p99 = avg((x) => x.p99_ms), p50 = avg((x) => x.p50_ms);
  if (p99 != null && p50 != null && p99 > 200 && p99 > p50 * 20) out.push({ level: "warn", icon: "clock", title: "A long tail of latency", body: [`p99 is ${fmt.ms(p99)} while p50 is ${fmt.ms(p50)}: a few queries cost far more than the rest. The Statements tab of Queries ranks them; a `, code("LIMIT"), ", a filter on a keyword field, or ", code("cluster_by"), " on the field they order by usually helps."] });
  const deleted = p.deleted_docs, docs = p.docs;
  if (docs && deleted / (docs + deleted) > 0.2) out.push({ level: "warn", icon: "trash", title: "Many deleted rows are still in segments", body: [`${fmt.pct(deleted / (docs + deleted), 0)} of the rows in segments are deleted. Queries skip them, but still read past them; merges drop them as they rewrite segments.`] });
  if (p.file_bytes && p.reusable_bytes / p.file_bytes > 0.3) out.push({ level: "info", icon: "disk", title: "Much of the file is free space", body: [`${fmt.bytes(p.reusable_bytes)} of ${fmt.bytes(p.file_bytes)} is free for reuse. `, code("POST /v1/admin/vacuum"), " (System, Operations) returns it to the file system."] });
  if (p.segments > 40 && !p.merges_running) out.push({ level: "info", icon: "layers", title: "Many small segments", body: [`${p.segments} segments, and no merge running. Each query reads every segment; the background merges bring the count down as rows are written.`] });
  if (p.memtable_docs > 200000) out.push({ level: "info", icon: "mem", title: "A large memtable", body: [`${fmt.si(p.memtable_docs)} rows are not in a segment yet: a restart replays them from the log. They are written as a segment when the memtable reaches its limit.`] });
  if (p.wal_bytes > 512 * 1024 * 1024) out.push({ level: "warn", icon: "disk", title: "The write-ahead log is large", body: [`${fmt.bytes(p.wal_bytes)} of log, which a restart replays. It empties at the next flush of the memtable.`] });
  if (p.disk_used_ratio > 0.8) out.push({ level: p.disk_used_ratio > 0.9 ? "bad" : "warn", icon: "disk", title: "The disk is filling", body: [`${fmt.pct(p.disk_used_ratio, 0)} used, ${fmt.bytes(p.disk_free_bytes)} free. A write that finds the disk full stops the server's writes until it restarts.`] });
  if (p.query_memory_ratio > 0.7) out.push({ level: "warn", icon: "mem", title: "Queries hold most of their memory budget", body: ["Running queries hold ", fmt.pct(p.query_memory_ratio, 0), " of ", code("--query-memory-mb"), ". Large GROUP BY and joins are the usual cause."] });
  if (avg((x) => x.write_queue) > 0 && L.max_queued_writes && max((x) => x.write_queue) > L.max_queued_writes * 0.5) out.push({ level: "warn", icon: "import", title: "Writes queue up", body: ["The writer thread falls behind: send rows in larger batches, or use ", code("/v1/stream"), "."] });
  if (p.replica_behind_max > 1000) out.push({ level: "warn", icon: "topology", title: "A replica is far behind", body: [`${fmt.si(p.replica_behind_max)} transactions behind. It catches up from the primary's memory, or is sent a copy when it is further behind than `, code("--ship-log-mb"), "."] });
  if (!L.slow_ms) out.push({ level: "info", icon: "flame", title: "Slow queries are not counted", body: ["Start the server with ", code("--slow-ms 200"), " (say) to keep the slow ones and log them."] });
  if (stmts?.length) {
    const top = stmts[0];
    if (top.share > 0.5 && top.calls > 10) out.push({ level: "info", icon: "queries", title: "One statement takes most of the time", body: [`${fmt.pct(top.share, 0)} of query time is one shape (${fmt.n(top.calls)} calls, mean ${fmt.ms(top.mean_ms)}): `, code(top.fingerprint.slice(0, 120))] });
    const big = stmts.find((x) => x.rows_per_call > 5000 && x.calls > 5);
    if (big) out.push({ level: "info", icon: "table", title: "A statement returns many rows per call", body: [`${fmt.si(big.rows_per_call)} rows per call: `, code(big.fingerprint.slice(0, 100)), ". Add a ", code("LIMIT"), " or aggregate on the server."] });
  }
  return out;
}
