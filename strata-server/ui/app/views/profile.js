// The profiler: any query's EXPLAIN ANALYZE. Run one with its CPU time
// measured, once or many times (each run's time, and the profile of the
// median one); or open the profile of a query that already ran, among the
// slow ones (under a threshold set here while the server runs) or the
// latest. A profile is the flame chart of every step, the operator tree
// with total and self time, CPU time and rows, the segments by thread,
// where the time went, and on a coordinator each shard's call.

import { h, icon, clear, fmt, api, store, toast, card, empty, seg, every, failure, table, pill, toggle, debounce, sqlBlock, copy } from "../core.js";
import { editor } from "../sqledit.js";
import { tables, cached } from "../catalog.js";
import { profileView, profileFigures } from "../profile.js";

export default { id: "profile", title: "Profiler", icon: "flame", group: "Workload", keywords: "profile profiler explain analyze flame chart cpu threads segments operators steps slow queries debug latency", render, onParams };

const TABS = [["query", "Profile a query"], ["slow", "Slow queries"], ["recent", "Latest queries"]];
const OUTCOMES = { ok: "ok", partial: "warn", error: "bad", timeout: "bad", rejected: "warn", cancelled: "info" };

let route = null;
function onParams(r) { route?.(r.params); }

function render(page, r) {
  const tabs = h("div", { class: "tabs", role: "tablist" });
  const body = h("div");
  page.append(h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Profiler"), h("p", {}, "Where a query's time goes: every step it took, on which segment, thread and shard, with its CPU time."))), tabs, body);
  let tab = null, stop = null, view = null;
  function go(params) {
    const t = params.get("tab") || (params.get("id") ? "recent" : "query");
    if (t !== tab) {
      tab = t; stop?.();
      clear(tabs, TABS.map(([k, v]) => h("button", { role: "tab", "aria-selected": String(k === t), onclick: () => { location.hash = "#/profile?tab=" + k; } }, v)));
      clear(body);
      view = t === "query" ? runner(body) : ended(body, t);
      stop = view.stop;
    }
    view.open?.(params);
  }
  route = go;
  go(r.params);
  return () => { stop?.(); route = null; };
}

// ---------------------------------------------------------------- run one

function runner(body) {
  let runs = store.get("profile.runs", 1);
  let sql = store.get("profile.sql", "");
  let paramsText = store.get("profile.params", "");
  const ed = editor({ value: sql, onChange: (v) => { sql = v; store.set("profile.sql", v); }, onRun: () => run(), catalog: cached, label: "SQL to profile" });
  const go = h("button", { class: "btn primary", title: "Run it with its CPU time measured (Ctrl+Enter)", onclick: () => run() }, icon("flame"), "Profile");
  const params = h("textarea", { class: "input mono", rows: 2, placeholder: '{"q": "laptop"}', style: { width: "100%" }, value: paramsText, oninput: debounce(() => { paramsText = params.value; store.set("profile.params", paramsText); }, 200) });
  const runsSeg = seg([[1, "once"], [5, "5 times"], [20, "20 times"]], runs, (v) => { runs = v; store.set("profile.runs", v); }, { title: "Run it more than once to see how its time varies; the profile shown is the median run's" });
  const strip = h("div");
  const out = h("div");
  body.append(
    card({ body: h("div", { class: "stack" },
      h("div", { class: "wb-ed", style: { height: "150px", border: "1px solid var(--line)", borderRadius: "var(--r-sm)", overflow: "hidden" } }, ed.el),
      h("details", { open: !!paramsText }, h("summary", { class: "small dim" }, "Parameters, as JSON (for :names in the SQL)"), params),
      h("div", { class: "row" }, go, runsSeg, h("span", { class: "sep" }), h("span", { class: "small muted" }, "Each run is a normal query: it takes a slot and shows in Queries.")),
    ), attrs: { style: { marginBottom: "14px" } } }),
    strip, out);
  if (!sql) tables().then(() => { if (!sql && cached()[0]) { const t = cached()[0]; const f = (t.short?.fields || []).find((x) => x.type === "text" && x.indexed !== false); ed.value = f ? `SELECT * FROM ${t.table} ORDER BY bm25(${f.name}, 'search') DESC LIMIT 10` : `SELECT * FROM ${t.table} LIMIT 10`; sql = ed.value; } }).catch(() => {});
  clear(out, empty("flame", "Profile a query", "Write a SELECT and press Profile: the server runs it with its CPU time measured and answers every step it took."));
  let busy = false;
  async function run() {
    if (busy) return;
    const text = (ed.value || "").trim().replace(/;\s*$/, "");
    if (!text) { toast("Write a query first"); return; }
    let p = {};
    if (paramsText.trim()) { try { p = JSON.parse(paramsText); } catch (e) { toast("The parameters are not JSON: " + e.message, "bad"); return; } }
    busy = true; go.disabled = true;
    const all = [];
    try {
      for (let i = 0; i < runs; i++) {
        clear(strip, h("div", { class: "small muted", style: { marginBottom: "10px" } }, `Running ${i + 1} of ${runs}…`));
        const t0 = performance.now();
        const j = await api("/v1/query", { body: { sql: text, params: p, profile: true } });
        j.roundtrip_ms = performance.now() - t0;
        all.push(j);
      }
      show(all, text);
    } catch (e) { clear(strip); clear(out, h("div", { class: "error" }, e.message)); }
    finally { busy = false; go.disabled = false; }
  }
  function show(all, text) {
    const order = all.map((j, i) => [j.took_ms, i]).sort((a, b) => a[0] - b[0]);
    let pick = order[Math.floor((order.length - 1) / 2)][1];
    function draw() {
      const j = all[pick];
      if (all.length > 1) {
        const t = order.map(([v]) => v), max = t[t.length - 1];
        const cpu = all.map((x) => profileFigures({ steps: x.stats?.steps, trace: x.trace }).cpu).filter((x) => x != null).sort((a, b) => a - b);
        clear(strip, card({ title: `${all.length} runs`, sub: "click a run for its profile", attrs: { style: { marginBottom: "14px" } }, body: h("div", { class: "row", style: { gap: "24px", alignItems: "flex-end" } },
          h("div", { class: "prof-runs", role: "list", style: { width: Math.min(560, all.length * 26) + "px" } }, all.map((x, i) => h("i", { role: "listitem", tabindex: 0, class: i === pick ? "sel" : "", title: `run ${i + 1}: ${fmt.ms(x.took_ms)}`, style: { height: Math.max(6, x.took_ms / max * 100) + "%", cursor: "pointer" }, onclick: () => { pick = i; draw(); }, onkeydown: (e) => { if (e.key === "Enter") { pick = i; draw(); } } }))),
          h("div", { class: "row small", style: { gap: "16px" } }, [["min", fmt.ms(t[0])], ["median", fmt.ms(t[Math.floor((t.length - 1) / 2)])], ["max", fmt.ms(max)], ["CPU, median", cpu.length ? fmt.ms(cpu[Math.floor((cpu.length - 1) / 2)] / 1000) : "–"]].map(([k, v]) => h("span", { class: "nowrap" }, h("span", { class: "muted" }, k + " "), h("b", {}, v))))) }));
      } else clear(strip);
      clear(out, h("div", { class: "stack" },
        h("div", { class: "row" }, h("span", { class: "small muted" }, all.length > 1 ? `run ${pick + 1} of ${all.length}` : "one run"), h("span", { class: "sep" }),
          h("a", { class: "btn sm", href: "#/sql?q=" + encodeURIComponent(text) }, icon("sql"), "Open in the workbench"),
          h("button", { class: "btn sm", onclick: () => copy(JSON.stringify({ sql: text, took_ms: j.took_ms, steps: j.stats?.steps, trace: j.trace, plan: j.stats?.plan }, null, 1), "Profile copied as JSON") }, icon("copy"), "Copy as JSON")),
        profileView({ steps: j.stats?.steps, steps_dropped: j.stats?.steps_dropped, trace: j.trace, took_ms: j.took_ms, roundtrip_ms: j.roundtrip_ms, plan: j.stats?.plan, rows: j.rows?.length, filter_docs: j.stats?.filter_docs, scored: j.stats?.scored, profiled: true })));
    }
    draw();
  }
  return {
    stop: () => {},
    open(params) { const q = params.get("sql"); if (q) { ed.value = q; sql = q; store.set("profile.sql", q); run(); } },
  };
}

// ---------------------------------------------------------------- ended ones

function ended(body, which) {
  const settings = h("div", { class: "prof-set" });
  const note = h("div", { class: "small muted" });
  const picked = h("div", { style: { marginTop: "14px" } });
  let sel = null;
  const tbl = table([
    { key: "ts_ms", label: "time", num: true, render: (q) => h("span", { class: "nowrap", title: fmt.datetime(q.ts_ms) }, fmt.time(q.ts_ms)) },
    { key: "outcome", label: "outcome", render: (q) => pill(OUTCOMES[q.outcome] || "info", q.outcome) },
    { key: "ms", label: "took", num: true, render: (q) => h("b", {}, fmt.ms(q.ms)) },
    { key: "cpu_ms", label: "CPU", num: true, sort: (q) => q.cpu_ms ?? -1, render: (q) => q.cpu_ms != null ? fmt.ms(q.cpu_ms) : h("span", { class: "muted", title: "Not measured: run it with profile on, or turn on CPU time for every query" }, "–") },
    { key: "steps_n", label: "steps", num: true, render: (q) => fmt.n(q.steps_n || 0) },
    { key: "rows", label: "rows", num: true, render: (q) => fmt.n(q.rows) },
    { key: "kind", label: "kind", render: (q) => h("span", { class: "badge mono" }, q.kind) },
    { key: "sql", label: "SQL", cls: "sql", render: (q) => h("span", { title: q.sql }, q.sql) },
  ], { sortKey: which === "slow" ? "ms" : "ts_ms", desc: true, max: 400, maxHeight: "340px", onRow: (q) => { location.hash = `#/profile?tab=${which}&id=${q.id}`; }, rowClass: (q) => q.id === sel ? "sel" : "",
    empty: empty("flame", which === "slow" ? "No slow queries" : "No queries yet", which === "slow" ? "Set a threshold above: every query that takes longer keeps its profile here." : "Run a query and its profile shows here.") });
  body.append(
    which === "slow" ? card({ title: "What is kept", sub: "set while the server runs; a restart goes back to its flags", body: settings, attrs: { style: { marginBottom: "14px" } } }) : null,
    h("div", { class: "row", style: { marginBottom: "10px" } }, note),
    card({ body: tbl, flush: true }),
    picked);
  if (which === "slow") drawSettings();
  async function drawSettings() {
    try {
      const s = await api("/v1/profile/settings");
      const inp = h("input", { class: "input sm num", type: "number", min: 0, step: "any", value: s.slow_ms ?? "", placeholder: "off" });
      const save = async (body) => {
        try { const r = await api("/v1/profile/settings", { method: "PUT", body }); toast(r.slow_ms == null ? "No query is kept as slow" : `Queries over ${fmt.ms(r.slow_ms)} are kept as slow`, "ok"); drawSettings(); load(); }
        catch (e) { toast(e.message, "bad"); }
      };
      clear(settings,
        h("label", { class: "row nw small dim", style: { gap: "8px" } }, "Keep queries slower than", inp, "ms"),
        h("button", { class: "btn sm primary", onclick: () => save({ slow_ms: inp.value === "" ? null : Number(inp.value) }) }, "Set"),
        h("button", { class: "btn sm", title: "Keep none as slow", onclick: () => save({ slow_ms: null }) }, "Off"),
        s.started_slow_ms !== s.slow_ms ? h("span", { class: "small muted" }, `started with ${s.started_slow_ms == null ? "none" : fmt.ms(s.started_slow_ms)}`) : null,
        h("span", { class: "sep" }),
        toggle(!!s.cpu_all, (v) => save({ cpu_all: v }), "Measure CPU time for every query (about 0.3 µs a step)"));
    } catch (e) { clear(settings, failure(e, "The profiling settings")); }
  }
  let all = [];
  async function load() {
    try {
      const j = await api(`/v1/queries/${which}?limit=500`);
      all = j.queries;
      tbl.update(all);
      const ms = all.map((q) => q.ms).sort((a, b) => a - b);
      clear(note, all.length ? `${fmt.plural(all.length, "query", "queries")} · median ${fmt.ms(ms[Math.floor(ms.length / 2)])} · slowest ${fmt.ms(ms[ms.length - 1])} · ${all.filter((q) => q.cpu_ms != null).length} with CPU time · click one for its profile` : "");
    } catch (e) { clear(body, failure(e, which === "slow" ? "The slow queries" : "The latest queries")); stopper(); }
  }
  async function open(id) {
    sel = id; tbl.update(all);
    clear(picked, h("div", { class: "small muted" }, "Reading its profile…"));
    try {
      const q = await api("/v1/queries/finished/" + id);
      clear(picked, h("div", { class: "stack" },
        h("div", { class: "row" }, pill(OUTCOMES[q.outcome] || "info", q.outcome), h("b", {}, `${fmt.ms(q.ms)} · ${q.kind}`), h("span", { class: "small muted" }, `${fmt.datetime(q.ts_ms)}${q.shard ? " · for a coordinator" : ""}`), h("span", { class: "sep" }),
          h("a", { class: "btn sm", href: "#/profile?tab=query&sql=" + encodeURIComponent(q.sql), title: "Run it again with its CPU time measured" }, icon("flame"), "Profile it again"),
          h("a", { class: "btn sm", href: "#/sql?q=" + encodeURIComponent(q.sql) }, icon("sql"), "Open in the workbench")),
        sqlBlock(q.sql),
        q.error ? h("div", { class: "error" }, q.error) : null,
        profileView({ ...q, took_ms: q.ms })));
    } catch (e) { clear(picked, e.status === 404 ? empty("clock", "That query's profile is gone", "The server keeps the latest 500 queries and the slowest; it ran too long ago.") : h("div", { class: "error" }, e.message)); }
  }
  const stopper = every(5000, load);
  return { stop: stopper, open(params) { const id = Number(params.get("id")); if (id && id !== sel) open(id); } };
}
