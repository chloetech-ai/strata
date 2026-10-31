// Queries: what runs now (with a button to cancel it), the latest and the
// slow ones on a scatter of time against latency, each with its trace as
// a waterfall (on a coordinator, every call to a shard replica), and the
// statements grouped by shape with what each costs.

import { h, s, icon, clear, fmt, card, api, every, drawer, toast, confirm, pill, empty, table, sqlBlock, copy, shareBar, failure, debounce, showTip, hideTip } from "../core.js";
import { state } from "../data.js";

export default { id: "queries", title: "Queries", icon: "queries", group: "Workload", keywords: "live running cancel recent slow statements trace profile waterfall pg_stat_statements", render, onParams };

const OUTCOMES = { ok: "ok", partial: "warn", error: "bad", timeout: "bad", rejected: "warn", cancelled: "info" };
const OCOLOR = { ok: "var(--s1)", partial: "var(--s4)", error: "var(--crit)", timeout: "var(--s8)", rejected: "var(--s2)", cancelled: "var(--s7)" };
const TABS = [["live", "Running now"], ["recent", "Latest"], ["slow", "Slow"], ["statements", "Statements"]];

let switchTab = null;
function onParams(r) { switchTab?.(r.params.get("tab") || "live"); }

/** A trace as a waterfall: a lane per step, bars on one time scale. */
export function waterfall(spans) {
  if (!spans?.length) return h("div", { class: "small muted" }, "No trace was kept for this query.");
  const end = Math.max(...spans.map((x) => x.start_us + x.dur_us), 1);
  const kind = (n) => n === "admission" ? "k-admission" : n === "execute" ? "k-execute" : n.startsWith("wait") ? "k-wait" : "k-rpc";
  const rows = spans.slice().sort((a, b) => a.start_us - b.start_us);
  return h("div", {},
    h("div", { class: "wf" }, rows.map((x) => [
      h("div", { class: "nm", title: x.replica || "" }, h("b", {}, x.name), x.shard != null ? h("span", { class: "badge" }, "shard " + x.shard) : null, x.again ? h("span", { class: "badge", title: "a hedge or a retry" }, "again") : null, x.outcome !== "ok" ? h("span", { class: "pill " + (x.outcome === "outrun" ? "info" : "bad"), style: { padding: "0 6px" } }, x.outcome) : null),
      h("div", { class: "lane" }, h("div", { class: `seg ${kind(x.name)}${x.outcome !== "ok" && x.outcome !== "outrun" ? " bad" : ""}${x.again ? " again" : ""}`, style: { left: (x.start_us / end * 100) + "%", width: Math.max(0.4, x.dur_us / end * 100) + "%" }, title: `${x.name}: ${fmt.ms(x.dur_us / 1000)} from ${fmt.ms(x.start_us / 1000)}${x.replica ? " on " + x.replica : ""}` },
        h("span", { class: "tiny" }, fmt.ms(x.dur_us / 1000)))),
    ]),
    h("div", { class: "scale" }, [0, 0.25, 0.5, 0.75, 1].map((f) => h("span", {}, fmt.ms(end * f / 1000))))));
}

/** The details of one ended query. */
export function queryDrawer(q) {
  const logs = h("div");
  const same = h("div");
  const d = drawer({
    title: h("span", { class: "row nw" }, pill(OUTCOMES[q.outcome] || "info", q.outcome), `${fmt.ms(q.ms)} · ${q.kind}`), sub: `${fmt.datetime(q.ts_ms)} · ${fmt.plural(q.rows, "row")}${q.shard ? " · for a coordinator" : ""}`, wide: true,
    body: [
      h("div", { class: "row between" }, h("b", {}, "SQL"), h("div", { class: "row" }, h("button", { class: "btn sm", onclick: () => copy(q.sql, "SQL copied") }, icon("copy"), "Copy"), h("a", { class: "btn sm", href: "#/sql?q=" + encodeURIComponent(q.sql), title: "Open it in the SQL workbench", onclick: () => d.close() }, icon("play"), "Open in the workbench"))),
      sqlBlock(q.sql),
      q.error ? h("div", { class: "error" }, q.error) : null,
      h("dl", { class: "kv" }, [["shape", h("span", { class: "mono small" }, q.fingerprint)], ["request id", q.request_id ? h("span", { class: "row nw" }, h("span", { class: "mono" }, q.request_id), h("button", { class: "btn ghost icon sm", title: "Copy", onclick: () => copy(q.request_id) }, icon("copy")), h("a", { class: "btn ghost sm", href: "#/logs?request=" + encodeURIComponent(q.request_id) }, "Its logs", icon("right"))) : "–"]].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
      card({ title: "Trace", sub: "where the time went", body: waterfall(q.trace) }),
      q.request_id ? card({ title: "Everything this request ran", body: same }) : null,
      q.request_id ? card({ title: "Its log lines", body: logs }) : null,
    ],
  });
  if (q.request_id) {
    api("/v1/queries/request/" + encodeURIComponent(q.request_id)).then((j) => clear(same, j.queries.length > 1 ? j.queries.map((x) => h("div", { class: "row nw small", style: { padding: "3px 0" } }, pill(OUTCOMES[x.outcome] || "info", x.outcome), h("span", { class: "mono ellipsis grow" }, x.sql), h("b", { class: "num" }, fmt.ms(x.ms)))) : h("div", { class: "small muted" }, "Just this query."))).catch((e) => clear(same, h("div", { class: "small muted" }, e.message)));
    api("/v1/logs?limit=50&request=" + encodeURIComponent(q.request_id)).then((j) => clear(logs, j.lines.length ? h("div", { class: "logs", style: { height: "auto", maxHeight: "240px", minHeight: 0 } }, j.lines.map((l) => h("div", { class: "ln" }, h("span", { class: "muted" }, fmt.time(l.ts_ms)), h("span", { class: "lv " + l.level }, l.level), h("span", { class: "tg" }, l.target), h("span", { class: "msg" }, l.message)))) : h("div", { class: "small muted" }, "No lines at the current log level. Set it to debug in Logs to see every request."))).catch((e) => clear(logs, h("div", { class: "small muted" }, e.message)));
  }
  return d;
}

function scatter(qs, onPick) {
  const W = 1000, H = 190, M = { l: 52, r: 12, t: 10, b: 22 };
  const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, style: { width: "100%", height: H + "px" }, role: "img", "aria-label": "latency of the latest queries" });
  if (!qs.length) return svgEl;
  const t0 = Math.min(...qs.map((q) => q.ts_ms)), t1 = Math.max(...qs.map((q) => q.ts_ms), t0 + 1000);
  const lo = Math.max(0.01, Math.min(...qs.map((q) => q.ms || 0.01))), hi = Math.max(...qs.map((q) => q.ms || 0.01), lo * 10);
  const ll = Math.log10(lo), lh = Math.log10(hi);
  const X = (t) => M.l + (t - t0) / (t1 - t0) * (W - M.l - M.r);
  const Y = (v) => M.t + (H - M.t - M.b) * (1 - (Math.log10(Math.max(v, lo)) - ll) / ((lh - ll) || 1));
  for (let e = Math.floor(ll); e <= Math.ceil(lh); e++) {
    const v = 10 ** e; if (v < lo / 1.01 || v > hi * 1.01) continue;
    svgEl.append(s("line", { x1: M.l, x2: W - M.r, y1: Y(v), y2: Y(v), style: { stroke: "var(--grid)" } }), s("text", { x: M.l - 6, y: Y(v) + 3.5, "text-anchor": "end", style: { fill: "var(--muted)", fontSize: "10.5px" } }, fmt.ms(v)));
  }
  for (let k = 0; k <= 4; k++) { const t = t0 + (t1 - t0) * k / 4; svgEl.append(s("text", { x: X(t), y: H - 6, "text-anchor": "middle", style: { fill: "var(--muted)", fontSize: "10.5px" } }, fmt.time(t))); }
  for (const q of qs) {
    const c = s("circle", { cx: X(q.ts_ms), cy: Y(q.ms), r: q.outcome === "ok" ? 4 : 5, tabindex: 0, style: { fill: OCOLOR[q.outcome] || "var(--s1)", stroke: "var(--surface)", strokeWidth: 1.5, cursor: "pointer", opacity: 0.85 } });
    const hit = s("circle", { cx: X(q.ts_ms), cy: Y(q.ms), r: 11, style: { fill: "transparent", cursor: "pointer" } });
    const tip = (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `${fmt.time(q.ts_ms)} · ${q.kind}`), h("div", { class: "r" }, h("span", { class: "k box", style: { background: OCOLOR[q.outcome] } }), h("b", {}, fmt.ms(q.ms)), h("span", { class: "n" }, `${q.outcome} · ${fmt.plural(q.rows, "row")}`)), h("div", { class: "mono", style: { marginTop: "4px", maxWidth: "340px", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" } }, q.sql)]);
    for (const el of [c, hit]) { el.addEventListener("pointermove", tip); el.addEventListener("pointerleave", hideTip); el.addEventListener("click", () => { hideTip(); onPick(q); }); }
    svgEl.append(c, hit);
  }
  return svgEl;
}

function render(page, r) {
  const tabs = h("div", { class: "tabs", role: "tablist" });
  const body = h("div");
  page.append(h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Queries"), h("p", {}, "Every query this server ran: the ones running, the latest 500, the slow ones, and their statistics by shape."))), tabs, body);
  let stop = null, tab = null;

  function show(t) {
    if (t === tab) return;
    tab = t;
    stop?.();
    clear(tabs, TABS.map(([k, v]) => h("button", { role: "tab", "aria-selected": String(k === t), onclick: () => { location.hash = "#/queries?tab=" + k; } }, v)));
    clear(body);
    stop = t === "live" ? live(body) : t === "statements" ? statements(body) : ended(body, t);
  }
  switchTab = show;
  show(r.params.get("tab") || "live");
  return () => { stop?.(); switchTab = null; };
}

function live(body) {
  const info = h("div", { class: "row", style: { gap: "12px", marginBottom: "10px" } });
  const tbl = table([
    { key: "id", label: "id", num: true, render: (q) => h("span", { class: "mono" }, q.id) },
    { key: "elapsed_ms", label: "running for", num: true, render: (q) => { const lim = state.system?.limits?.timeout_ms || 5000; return h("div", { style: { minWidth: "130px" } }, h("div", { class: "row nw between small" }, h("b", {}, fmt.ms(q.elapsed_ms)), h("span", { class: "muted" }, fmt.pct(Math.min(1, q.elapsed_ms / lim), 0) + " of timeout")), h("div", { class: "meter", style: { marginTop: "3px" } }, h("i", { class: q.elapsed_ms > lim * 0.8 ? "bad" : q.elapsed_ms > lim * 0.5 ? "warn" : "", style: { width: Math.min(100, q.elapsed_ms / lim * 100) + "%" } }))); } },
    { key: "kind", label: "kind", render: (q) => h("span", { class: "badge mono" }, q.kind) },
    { key: "sql", label: "SQL", cls: "sql", render: (q) => h("span", { title: q.sql }, q.sql) },
    { key: "shard", label: "from", render: (q) => q.shard ? h("span", { class: "badge" }, "coordinator") : "client" },
    { key: "act", label: "", sortable: false, render: (q) => q.cancelled ? pill("info", "cancelling") : h("button", { class: "btn danger sm", onclick: async () => {
      if (!await confirm({ title: `Cancel query ${q.id}?`, body: sqlBlock(q.sql), ok: "Cancel it", danger: true })) return;
      try { await api("/v1/queries/" + q.id, { method: "DELETE" }); toast(`Query ${q.id} cancelled; it answers 499`, "ok"); load(); } catch (e) { toast(e.message, "bad"); }
    } }, icon("stop"), "Cancel") },
  ], { sortKey: "elapsed_ms", desc: true, empty: empty("okc", "Nothing running", "A query shows here while it runs, with how long it has run against its timeout."), onRow: (q) => drawer({ title: `Query ${q.id}`, sub: `running for ${fmt.ms(q.elapsed_ms)}`, wide: true, body: [sqlBlock(q.sql), card({ title: "Trace so far", body: waterfall(q.trace) })] }) });
  body.append(info, card({ body: tbl, flush: true }));
  async function load() {
    try {
      const j = await api("/v1/queries");
      clear(info, pill(j.queries.length ? "info" : "ok", `${j.queries.length} running`), state.system ? h("span", { class: "small muted" }, `${j.running} holding slots of ${state.system.limits.max_concurrent} · timeout ${fmt.ms(state.system.limits.timeout_ms)}`) : null, h("span", { class: "small muted" }, "refreshes every second"));
      tbl.update(j.queries);
    } catch (e) { clear(body, failure(e, "The running queries")); stopper(); }
  }
  const stopper = every(1000, load);
  return stopper;
}

function ended(body, which) {
  const filt = { outcome: null, kind: null, q: "", shard: true };
  const chips = h("div", { class: "row" });
  const search = h("input", { class: "input sm", placeholder: "Filter by SQL…", style: { width: "240px" }, oninput: debounce(() => { filt.q = search.value.toLowerCase(); draw(); }, 150) });
  const kindSel = h("select", { class: "select sm", onchange: () => { filt.kind = kindSel.value || null; draw(); } });
  const plot = h("div");
  const note = h("div", { class: "small muted" });
  const tbl = table([
    { key: "ts_ms", label: "time", num: true, render: (q) => h("span", { class: "nowrap", title: fmt.datetime(q.ts_ms) }, fmt.time(q.ts_ms)) },
    { key: "outcome", label: "outcome", render: (q) => pill(OUTCOMES[q.outcome] || "info", q.outcome) },
    { key: "kind", label: "kind", render: (q) => h("span", { class: "badge mono" }, q.kind) },
    { key: "ms", label: "took", num: true, render: (q) => h("b", {}, fmt.ms(q.ms)) },
    { key: "rows", label: "rows", num: true, render: (q) => fmt.n(q.rows) },
    { key: "sql", label: "SQL", cls: "sql", render: (q) => h("span", { title: q.sql }, q.sql) },
    { key: "trace", label: "steps", num: true, sort: (q) => q.trace?.length || 0, render: (q) => q.trace?.length ? miniTrace(q.trace) : "–" },
  ], { sortKey: "ts_ms", desc: true, onRow: queryDrawer, empty: empty("queries", which === "slow" ? "No slow queries" : "No queries yet", which === "slow" ? (state.system?.limits?.slow_ms ? `None took over ${state.system.limits.slow_ms} ms.` : "Start the server with --slow-ms to keep them.") : "Run a query and it shows here.") });
  body.append(
    h("div", { class: "row", style: { marginBottom: "12px", gap: "10px" } }, chips, kindSel, search, toggleShard(), h("span", { class: "sep" }), note),
    card({ title: "Latency of each query", sub: "log scale · click a dot for its trace", body: plot, attrs: { style: { marginBottom: "14px" } } }),
    card({ body: tbl, flush: true }));
  function toggleShard() { const b = h("button", { class: "chip", "aria-pressed": "true", title: "Queries a coordinator sent this shard", onclick: () => { filt.shard = !filt.shard; b.setAttribute("aria-pressed", String(filt.shard)); draw(); } }, "shard calls"); return b; }
  let all = [];
  function draw() {
    const outs = {}; for (const q of all) outs[q.outcome] = (outs[q.outcome] || 0) + 1;
    clear(chips, Object.entries(outs).map(([o, n]) => { const c = h("button", { class: "chip", "aria-pressed": String(filt.outcome === o), onclick: () => { filt.outcome = filt.outcome === o ? null : o; draw(); } }, h("span", { class: "dot", style: { background: OCOLOR[o] } }), `${o} ${n}`); return c; }));
    const kinds = [...new Set(all.map((q) => q.kind))].sort();
    if (kindSel.options.length !== kinds.length + 1) { clear(kindSel, h("option", { value: "" }, "every kind"), kinds.map((k) => h("option", { value: k }, k))); kindSel.value = filt.kind || ""; }
    const rows = all.filter((q) => (!filt.outcome || q.outcome === filt.outcome) && (!filt.kind || q.kind === filt.kind) && (!filt.q || q.sql.toLowerCase().includes(filt.q)) && (filt.shard || !q.shard));
    clear(plot, scatter(rows, queryDrawer));
    tbl.update(rows);
    const ms = rows.map((q) => q.ms).sort((a, b) => a - b);
    note.textContent = rows.length ? `${fmt.plural(rows.length, "query", "queries")} · median ${fmt.ms(ms[Math.floor(ms.length / 2)])} · slowest ${fmt.ms(ms[ms.length - 1])}` : "";
  }
  async function load() {
    try { const j = await api(`/v1/queries/${which}?limit=500`); all = j.queries; draw(); }
    catch (e) { clear(body, failure(e, which === "slow" ? "The slow queries" : "The latest queries")); stopper(); }
  }
  const stopper = every(4000, load);
  return stopper;
}

function miniTrace(spans) {
  const end = Math.max(...spans.map((x) => x.start_us + x.dur_us), 1);
  const svgEl = s("svg", { viewBox: "0 0 80 14", width: 80, height: 14, "aria-hidden": "true" });
  spans.slice(0, 12).forEach((x, i) => svgEl.append(s("rect", { x: x.start_us / end * 80, y: (i % 3) * 4.5, width: Math.max(1, x.dur_us / end * 80), height: 4, rx: 1, style: { fill: x.outcome !== "ok" && x.outcome !== "outrun" ? "var(--crit)" : x.name === "admission" ? "var(--s4)" : x.name === "execute" ? "var(--s1)" : "var(--s3)" } })));
  return svgEl;
}

function statements(body) {
  const head = h("div", { class: "row", style: { marginBottom: "12px" } });
  const tbl = table([
    { key: "fingerprint", label: "statement", cls: "sql", render: (x) => h("span", { title: x.example }, x.fingerprint) },
    { key: "kind", label: "kind", render: (x) => h("span", { class: "badge mono" }, x.kind) },
    { key: "calls", label: "calls", num: true, render: (x) => fmt.n(x.calls) },
    { key: "errors", label: "errors", num: true, render: (x) => x.errors ? h("span", { style: { color: "var(--crit-ink)" } }, fmt.n(x.errors)) : "0" },
    { key: "total_ms", label: "total", num: true, render: (x) => fmt.ms(x.total_ms) },
    { key: "mean_ms", label: "mean", num: true, render: (x) => fmt.ms(x.mean_ms) },
    { key: "p95_ms", label: "p95", num: true, render: (x) => fmt.ms(x.p95_ms) },
    { key: "p99_ms", label: "p99", num: true, render: (x) => fmt.ms(x.p99_ms) },
    { key: "rows_per_call", label: "rows/call", num: true, render: (x) => fmt.si(x.rows_per_call) },
    { key: "share", label: "share of time", num: true, render: (x) => shareBar(x.share) },
  ], { sortKey: "total_ms", desc: true, onRow: stmtDrawer, empty: empty("queries", "No statements yet", "Queries are grouped by shape: the same SQL with other literals.") });
  const reset = h("button", { class: "btn sm danger", onclick: async () => {
    if (!await confirm({ title: "Reset the statement statistics?", body: "Every count starts again from zero. The latest queries are kept.", ok: "Reset", danger: true })) return;
    try { await api("/v1/queries/stats/reset", { method: "POST", body: {} }); toast("Statistics reset", "ok"); load(); } catch (e) { toast(e.message, "bad"); }
  } }, icon("refresh"), "Reset");
  body.append(head, card({ body: tbl, flush: true }));
  async function load() {
    try {
      const j = await api("/v1/queries/stats?limit=500");
      const total = j.statements.reduce((a, x) => a + x.total_ms, 0), calls = j.statements.reduce((a, x) => a + x.calls, 0);
      clear(head, pill("info", fmt.plural(j.count, "shape")), h("span", { class: "small muted" }, `${fmt.n(calls)} calls · ${fmt.ms(total)} in all · since ${fmt.datetime(j.since_ms)}`), h("span", { class: "sep" }), reset);
      tbl.update(j.statements);
    } catch (e) { clear(body, failure(e, "The statement statistics")); stopper(); }
  }
  const stopper = every(5000, load);
  return stopper;
}

function stmtDrawer(x) {
  drawer({
    title: `${fmt.n(x.calls)} calls · ${fmt.ms(x.mean_ms)} mean`, sub: `${x.kind} · ${fmt.pct(x.share)} of the time · last ${fmt.ago(x.last_ms)}`, wide: true,
    body: [
      h("b", {}, "The shape"), sqlBlock(x.fingerprint),
      h("b", {}, "The latest example"), h("div", { class: "row nw" }, h("div", { class: "grow" }, sqlBlock(x.example)), h("button", { class: "btn icon sm", title: "Copy", onclick: () => copy(x.example) }, icon("copy"))),
      h("div", { class: "grid g4" }, [["min", x.min_ms], ["p50", x.p50_ms], ["p95", x.p95_ms], ["p99", x.p99_ms], ["max", x.max_ms], ["mean", x.mean_ms], ["total", x.total_ms]].map(([k, v], i) => h("div", { class: "card kpi" }, h("div", { class: "lbl" }, k), h("div", { class: "val", style: { fontSize: "20px", color: i === 3 ? "var(--text)" : null } }, fmt.ms(v))))),
      h("dl", { class: "kv" }, [["errors", fmt.n(x.errors)], ["rows", `${fmt.n(x.rows)} (${fmt.si(x.rows_per_call)} per call)`], ["calls from a coordinator", fmt.n(x.shard_calls)]].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
    ],
  });
}
