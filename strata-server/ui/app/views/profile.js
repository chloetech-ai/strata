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
import { profileView, profileFigures, kpi } from "../profile.js";
import { flameGraph, folded, shortName } from "../flame.js";

export default { id: "profile", title: "Profiler", icon: "flame", group: "Workload", keywords: "profile profiler explain analyze flame chart flame graph cpu sampling pprof perf folded speedscope hot functions threads segments operators steps slow queries debug latency", render, onParams };

const TABS = [["query", "Profile a query"], ["cpu", "CPU"], ["slow", "Slow queries"], ["recent", "Latest queries"]];
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
      view = t === "query" ? runner(body) : t === "cpu" ? cpu(body) : ended(body, t);
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

// ---------------------------------------------------------------- CPU

const SECONDS = [[1, "1 s"], [5, "5 s"], [10, "10 s"], [30, "30 s"]];
const HZ = [[49, "49 Hz"], [99, "99 Hz"], [499, "499 Hz"], [997, "997 Hz"]];
const CLIENTS = [[1, "1"], [4, "4"], [16, "16"]];

/**
 * A CPU profile: every thread of the server sampled for a while, what it
 * was running, as a flame graph and by function. Of the whole server
 * (whatever its clients send), or of one query run again and again.
 */
function cpu(body) {
  let what = store.get("profile.cpu.what", "server");
  let seconds = store.get("profile.cpu.seconds", 5);
  let hz = store.get("profile.cpu.hz", 99);
  let clients = store.get("profile.cpu.clients", 1);
  let sql = store.get("profile.cpu.sql", store.get("profile.sql", ""));
  const ed = editor({ value: sql, onChange: (v) => { sql = v; store.set("profile.cpu.sql", v); }, onRun: () => take(), catalog: cached, label: "SQL to run while sampling" });
  const qbox = h("div", { class: "stack" },
    h("div", { class: "wb-ed", style: { height: "110px", border: "1px solid var(--line)", borderRadius: "var(--r-sm)", overflow: "hidden" } }, ed.el),
    h("div", { class: "row small dim" }, "Run by", seg(CLIENTS, clients, (v) => { clients = v; store.set("profile.cpu.clients", v); }, { title: "How many callers run it at once: 1 shows one query's work, more shows the server under load" }), "callers at once, again and again until the profile ends. Its runs are not kept among the latest queries."));
  const showQ = () => { qbox.style.display = what === "query" ? "" : "none"; };
  const go = h("button", { class: "btn primary", onclick: () => take() }, icon("flame"), "Take a CPU profile");
  const out = h("div");
  body.append(
    card({ body: h("div", { class: "stack" },
      h("div", { class: "row", style: { flexWrap: "wrap", gap: "14px" } },
        seg([["server", "the whole server"], ["query", "a query in a loop"]], what, (v) => { what = v; store.set("profile.cpu.what", v); showQ(); }, { title: "Sample whatever the server is doing, or one query run again and again" }),
        h("span", { class: "row nw small dim", style: { gap: "6px" } }, "for", seg(SECONDS, seconds, (v) => { seconds = v; store.set("profile.cpu.seconds", v); })),
        h("span", { class: "row nw small dim", style: { gap: "6px" }, title: "Samples a second of each thread's CPU time: more is finer and costs the server more (at 99 Hz well under 1%)" }, "at", seg(HZ, hz, (v) => { hz = v; store.set("profile.cpu.hz", v); }))),
      qbox,
      h("div", { class: "row" }, go, h("span", { class: "sep" }), h("span", { class: "small muted" }, "One profile at a time. The stacks are sampled with SIGPROF; frames are named from the binary's symbols."))),
    attrs: { style: { marginBottom: "14px" } } }),
    out);
  showQ();
  if (!sql) tables().then(() => { if (!sql && cached()[0]) { const t = cached()[0]; const f = (t.short?.fields || []).find((x) => x.type === "text" && x.indexed !== false); ed.value = f ? `SELECT * FROM ${t.table} ORDER BY bm25(${f.name}, 'search') DESC LIMIT 10` : `SELECT * FROM ${t.table} LIMIT 10`; sql = ed.value; } }).catch(() => {});
  clear(out, empty("flame", "Sample the server's CPU", "Take a profile of the whole server while it serves its clients, or of a query run in a loop: every thread's stacks, as a flame graph and the functions that took the most time."));
  let busy = false, tick = null, gone = false;
  async function take() {
    if (busy) return;
    const req = { seconds, hz };
    if (what === "query") {
      const text = (ed.value || "").trim().replace(/;\s*$/, "");
      if (!text) { toast("Write a query first"); return; }
      req.sql = text; req.clients = clients;
    }
    busy = true; go.disabled = true;
    const t0 = performance.now();
    const bar = h("i", { style: { width: "0%" } });
    const status = h("div", { class: "small muted" }, `${seconds} s left`);
    clear(out, card({ body: h("div", { class: "stack", style: { alignItems: "center", padding: "26px 0" } },
      h("div", { class: "small dim" }, what === "query" ? `Running the query in a loop and sampling every thread at ${hz} Hz…` : `Sampling every thread at ${hz} Hz…`),
      h("div", { class: "meter", style: { width: "min(420px, 90%)" } }, bar), status) }));
    tick = setInterval(() => { const el = (performance.now() - t0) / 1000; bar.style.width = Math.min(100, el / seconds * 100) + "%"; status.textContent = el < seconds ? `${Math.ceil(seconds - el)} s left` : "Reading the frames' names…"; }, 200);
    try {
      const p = await api("/v1/profile/cpu", { body: req });
      if (gone) return;
      show(p, req);
    } catch (e) {
      if (gone) return;
      clear(out, e.status === 409 ? empty("clock", "A profile is being taken", "One at a time: the timer is the process's. Try again when it ends.") : e.status === 501 ? empty("info", "No CPU profiles here", e.message) : h("div", { class: "error" }, e.message));
    } finally { clearInterval(tick); busy = false; go.disabled = false; }
  }
  function show(p, req) {
    const total = p.stacks.reduce((a, s) => a + s.samples, 0);
    // A sample stands for its share of the process's measured CPU time:
    // the system's timer delivers fewer than asked when the process is busy.
    const per = p.samples && p.cpu_seconds ? p.cpu_seconds * 1000 / p.samples : 1000 / p.hz;
    const unit = (n) => fmt.ms(n * per);
    const stamp = new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-");
    const save = (name, text, type) => { const a = h("a", { href: URL.createObjectURL(new Blob([text], { type })), download: name }); document.body.append(a); a.click(); setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 1000); };
    const fg = flameGraph(p, { unit });
    const r = p.runs;
    const top = p.threads[0];
    const few = p.rate && p.rate < p.hz * 0.6;
    // The function most samples were in, itself.
    const self = new Map();
    for (const s of p.stacks) { const f = s.frames[s.frames.length - 1]; if (f != null) self.set(f, (self.get(f) || 0) + s.samples); }
    const best = [...self].sort((x, y) => y[1] - x[1])[0];
    const hot = best ? [p.names[best[0]], best[1]] : null;
    clear(out, h("div", { class: "stack" },
      h("div", { class: "row" }, h("span", { class: "small muted" }, `${req.sql ? "a query in a loop" : "the whole server"} · ${fmt.n(p.seconds)} s at ${p.hz} Hz · ${new Date().toLocaleTimeString()}`), h("span", { class: "sep" }),
        h("button", { class: "btn sm", title: "Folded stacks (thread;root;…;leaf samples) for flamegraph.pl, inferno or speedscope", onclick: () => save(`strata-cpu-${stamp}.folded`, folded(p), "text/plain") }, icon("download"), "Folded"),
        h("button", { class: "btn sm", onclick: () => save(`strata-cpu-${stamp}.json`, JSON.stringify(p), "application/json") }, icon("download"), "JSON"),
        req.sql ? h("a", { class: "btn sm", href: "#/profile?tab=query&sql=" + encodeURIComponent(req.sql), title: "Its steps, once" }, icon("flame"), "Profile its steps") : null),
      req.sql ? sqlBlock(req.sql) : null,
      r?.error ? h("div", { class: "error" }, "A run failed, so the loop stopped: " + r.error) : null,
      h("div", { class: "grid g6 prof-kpis" },
        kpi("CPU time", fmt.ms(p.cpu_seconds * 1000), `of every thread, in ${fmt.n(p.seconds)} s`, "The process's CPU time (user and system) while the profile was taken, from the system"),
        kpi("Cores busy", p.cores.toFixed(2), `of ${fmt.n(p.seconds)} s`, "CPU seconds over wall seconds: the cores the process kept busy on average"),
        kpi("Samples", fmt.n(p.samples), `${fmt.n(Math.round(p.rate || 0))} a CPU second`, `${p.hz} asked for a CPU second; each sample stands for ${fmt.ms(per)} of CPU time`),
        kpi("Threads", fmt.n(p.threads.reduce((a, t) => a + t.count, 0)), top ? `${top.name} ${fmt.pct(top.samples / Math.max(1, total), 0)}` : null, "Threads sampled at least once; the busiest pool"),
        r ? kpi("Runs", fmt.n(r.count), r.count ? `${fmt.n(Math.round(r.count / p.seconds))} a second${r.clients > 1 ? `, ${r.clients} callers` : ""}` : null) : kpi("Stacks", fmt.n(p.stacks.length), `${fmt.n(p.names.length)} functions`),
        r ? kpi("Median run", r.median_ms != null ? fmt.ms(r.median_ms) : "–", r.p99_ms != null ? `p99 ${fmt.ms(r.p99_ms)} · max ${fmt.ms(r.max_ms)}` : null) : hot ? kpi("Hottest function", h("span", { class: "mono", style: { fontSize: "15px" } }, shortName(hot[0])), `${fmt.pct(hot[1] / Math.max(1, total), 1)} of the samples in itself`, hot[0]) : kpi("Stacks", "–")),
      few ? h("div", { class: "small muted" }, `The system delivered ${fmt.n(Math.round(p.rate))} samples a CPU second of the ${p.hz} asked: its timer gives the whole process at most one a tick (250 a second on many Linux kernels). Each sample is weighed by the measured CPU time, so the times are right; fewer samples make small functions noisier, so sample for longer.`) : null,
      (p.blocked || []).includes("libc") ? h("div", { class: "small muted" }, "Samples inside the C library (malloc, memcpy, system calls) are not taken on this system: unwinding from there could deadlock before glibc 2.35. Their CPU time is spread over the rest.") : null,
      !total ? empty("info", "No samples", "No thread used the CPU while the profile was taken. Sample for longer, or while the server is busy.") : null,
      total ? card({ title: "Flame graph", sub: "each frame as wide as its CPU time, what it called under it; click one to zoom in", body: fg.graph }) : null,
      total ? card({ title: "Functions", sub: "self: CPU time in the function itself · total: with what it called · click one to find it in the graph", body: fg.functions, flush: true }) : null));
  }
  return { stop: () => { gone = true; clearInterval(tick); } };
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
