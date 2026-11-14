// The profiler: diagnostics of one server. A query's EXPLAIN ANALYZE, run
// once or many times with its parameters (each run's time, and the
// profile of the median one); the CPU by function and the heap by where it
// was allocated, of the coordinator or any copy (picked by its short name,
// node or shard, with its load); its threads; and what this server keeps
// (its runtime settings). A rate above the safe defaults asks first and
// says what it costs. Queries that already ran are in Queries > History.

import { h, icon, clear, fmt, api, store, toast, card, empty, seg, every, failure, table, pill, toggle, debounce, sqlBlock, copy, bus, shareBar, confirm, needs } from "../core.js";
import { timeChart, SERIES } from "../charts.js";
import { visible, last, domain, setZoom } from "../data.js";
import { editor } from "../sqledit.js";
import { tables, cached } from "../catalog.js";
import { profileView, profileFigures, kpi } from "../profile.js";
import { flameGraph, folded, shortName } from "../flame.js";
import { state, link, copyName, copyTitle, copyLink, isCoord, queryDrawer, narrow, workbenchLink, profileLink, copies, statementLink, containRules, can } from "../workload.js";
import { isProduction, clusterName, environment } from "../data.js";

export default { id: "profile", title: "Profiler", icon: "flame", group: "Diagnose", keywords: "profile profiler diagnostics node explain analyze flame chart flame graph cpu sampling pprof perf folded speedscope hot functions memory heap allocations allocator leak rss threads context switches segments operators steps debug latency settings slow threshold", render, onParams };

const TABS = [["query", "Profile a query"], ["cpu", "CPU"], ["memory", "Memory"], ["settings", "What is kept"]];
/** The old Slow and Latest tabs are Queries > History with a filter. */
const MOVED = { slow: "slow", recent: "latest" };

let route = null;
function onParams(r) { route?.(r.params); }

function render(page, r) {
  const tabs = h("div", { class: "tabs wl-tabs-scroll", role: "tablist" });
  const body = h("div");
  page.append(h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Profiler"), h("p", {}, "Diagnostics of one server: a query's steps, its CPU by function, its heap by where it was allocated, its threads. Open it from a copy in Cluster to profile that copy."))), tabs, body);
  let tab = null, node = null, stop = null, view = null;
  function go(params) {
    let t = params.get("tab") || "query";
    if (MOVED[t]) { location.replace(link("queries", { tab: "history", preset: MOVED[t] })); return; }
    // An old link to a query that ran: its drawer, over the query tab.
    const id = Number(params.get("id"));
    if (id) { api("/v1/queries/finished/" + id).then((q) => queryDrawer({ ...q, id })).catch((e) => toast(e.status === 404 ? "That query's profile is gone: Queries > History keeps 7 days of them" : e.message, "bad")); t = "query"; }
    if (!TABS.some(([k]) => k === t)) t = "query";
    // On a coordinator, the CPU and Memory tabs profile any server of its
    // cluster: ?node= names it (a replica's URL), none the coordinator.
    // NEW-workload-10: a copy's name (s0-1) or its node name works too.
    const n = t === "cpu" || t === "memory" ? nodeUrl(params.get("node") || "") : "";
    if (t !== tab || n !== node) {
      tab = t; node = n; stop?.();
      clear(tabs, TABS.map(([k, v]) => h("button", { role: "tab", "aria-selected": String(k === t), onclick: () => { location.hash = link("profile", { tab: k, node: node && (k === "cpu" || k === "memory") ? node : null }); } }, v)),
        narrow() ? null : [h("span", { class: "sep" }), h("a", { class: "small wl-link", style: { alignSelf: "center" }, href: link("queries", { tab: "history" }) }, "Queries that ran: Queries > History ›")]);
      clear(body);
      if (t === "cpu" || t === "memory") body.append(nodePicker(t, n));
      view = t === "query" ? runner(body) : t === "cpu" ? cpu(body, n) : t === "memory" ? memory(body, n) : settingsTab(body);
      stop = view.stop;
    }
    view.open?.(params);
  }
  route = go;
  go(r.params);
  // A copy's name resolves once the cluster is read.
  const offCluster = bus.on("cluster", () => { const p = new URLSearchParams(location.hash.split("?")[1] || ""); const v = p.get("node"); if (v && !/^https?:\/\//.test(v) && nodeUrl(v) !== v && node !== nodeUrl(v)) go(p); });
  return () => { stop?.(); route = null; offCluster?.(); };
}

// ---------------------------------------------------------------- run one

function runner(body) {
  let runs = store.get("profile.runs", 1);
  let sql = store.get("profile.sql", "");
  let paramsText = store.get("profile.params", "");
  const ed = editor({ value: sql, onChange: (v) => { sql = v; store.set("profile.sql", v); }, onRun: () => run(), catalog: cached, label: "SQL to profile" });
  const go = h("button", { class: "btn primary", title: "Run it with its CPU time measured (Ctrl+Enter)", onclick: () => run() }, icon("flame"), "Profile");
  const params = h("textarea", { class: "input mono", rows: 2, placeholder: '{"q": "laptop"}', style: { width: "100%" }, value: paramsText, oninput: debounce(() => { paramsText = params.value; store.set("profile.params", paramsText); }, 200) });
  const params_ = params;
  // On a coordinator each run asks every shard: say how many calls before running.
  const runNote = h("span", { class: "small muted" });
  const drawNote = () => {
    const n = isCoord() ? state.cluster?.shards?.length || 0 : 0;
    clear(runNote, n ? `Each run is a normal query that asks every shard: ${runs === 1 ? "one run" : runs + " runs"} × ${fmt.n(n)} shards = ${fmt.n(runs * n)} shard calls. Runs show in Queries.` : "Each run is a normal query: it takes a slot and shows in Queries.");
  };
  const runsSeg = seg([[1, "once"], [5, "5 times"], [20, "20 times"]], runs, (v) => { runs = v; store.set("profile.runs", v); drawNote(); drawLinked?.(); }, { title: "Run it more than once to see how its time varies; the profile shown is the median run's" });
  const strip = h("div");
  const out = h("div");
  // What a link filled in, above the form: it has not run.
  const lead = h("div");
  body.append(lead,
    card({ body: h("div", { class: "stack" },
      h("div", { class: "wb-ed", style: { height: "150px", border: "1px solid var(--line)", borderRadius: "var(--r-sm)", overflow: "hidden" } }, ed.el),
      h("details", { open: !!paramsText }, h("summary", { class: "small dim wl-sum" }, "Parameters, as JSON (for :names in the SQL)"), params),
      h("div", { class: "row", style: { flexWrap: "wrap" } }, go, runsSeg, h("span", { class: "sep" }), runNote),
    ), attrs: { style: { marginBottom: "14px" } } }),
    strip, out);
  drawNote();
  const offCluster = bus.on("cluster", () => { if (runNote.isConnected) drawNote(); });
  if (!sql) tables().then(() => { if (!sql && cached()[0]) { const t = cached()[0]; const f = (t.short?.fields || []).find((x) => x.type === "text" && x.indexed !== false); ed.value = f ? `SELECT * FROM ${t.table} ORDER BY bm25(${f.name}, 'search') DESC LIMIT 10` : `SELECT * FROM ${t.table} LIMIT 10`; sql = ed.value; } }).catch(() => {});
  clear(out, empty("flame", "Profile a query", "Write a SELECT and press Profile: the server runs it with its CPU time measured and answers every step it took."));
  let busy = false;
  // A link fills the form and never runs it: the query is the link's, the
  // key and the cluster are whoever opens it. `linked` is what we know of
  // its statement (p99, contain rules), to ask before a slow or contained one.
  let linked = null;
  const drawLinked = () => clear(lead, linked ? linkCard(linked, runs, () => run()) : null);
  async function run() {
    if (busy) return;
    const text = (ed.value || "").trim().replace(/;\s*$/, "");
    if (!text) { toast("Write a query first"); return; }
    let p = {};
    if (paramsText.trim()) { try { p = JSON.parse(paramsText); } catch (e) { toast("The parameters are not JSON: " + e.message, "bad"); return; } }
    if (linked && text === linked.sql) {
      const risk = linkRisk(linked);
      if (risk.length) {
        const ok = await confirm({ title: linked.refusedAll ? "Run a statement that is refused?" : "Run this statement on " + (clusterName() || "this cluster") + "?", ok: runs === 1 ? "Run it once" : `Run it ${runs} times`,
          body: h("div", { class: "stack" }, h("ul", { class: "wl-list" }, risk.map((x) => h("li", {}, x))),
            h("p", { class: "small muted", style: { margin: 0 } }, `It runs ${runs === 1 ? "once" : runs + " times"} as a normal query under your key${isProduction() ? ", on production" : ""}, and shows in Queries.`)) });
        if (!ok) return;
      }
    }
    if (linked) { linked = null; drawLinked(); }
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
          h("a", { class: "btn sm", href: workbenchLink(text, paramsText.trim() ? paramsText : null) }, icon("sql"), "Open in the workbench"),
          h("button", { class: "btn sm", onclick: () => copy(JSON.stringify({ sql: text, took_ms: j.took_ms, steps: j.stats?.steps, trace: j.trace, plan: j.stats?.plan }, null, 1), "Profile copied as JSON") }, icon("copy"), "Copy as JSON")),
        profileView({ steps: j.stats?.steps, steps_dropped: j.stats?.steps_dropped, trace: j.trace, took_ms: j.took_ms, roundtrip_ms: j.roundtrip_ms, plan: j.stats?.plan, rows: j.rows?.length, filter_docs: j.stats?.filter_docs, scored: j.stats?.scored, profiled: true })));
    }
    draw();
  }
  /** The link's statement: filled in, not run, with what it does now. */
  async function fromLink(text, fp) {
    linked = { sql: text.trim().replace(/;\s*$/, ""), fp, stat: null, rules: [], refusedAll: false, read: !fp };
    const draw = drawLinked;
    draw();
    if (!fp) return;
    const to = Date.now(), from = to - 3600e3;
    const [st, rules] = await Promise.all([
      can("admin") ? api(`/v1/queries/stats/${encodeURIComponent(fp)}?from=${from}&to=${to}`).catch(() => null) : null,
      containRules().catch(() => null)]);
    if (!linked || linked.fp !== fp) return;
    linked.stat = st?.statement || null;
    const mine = state.whoami?.key ? state.whoami.name : null;
    linked.rules = (rules || []).filter((r) => r.match?.fingerprint === fp || r.match?.fingerprint === linked.stat?.fingerprint || (!r.match?.fingerprint && r.match?.key && r.match.key === mine));
    linked.refusedAll = linked.rules.some((r) => r.action === "refuse" && (!r.match?.key || r.match.key === mine));
    linked.read = true;
    draw();
  }
  return {
    stop: () => offCluster?.(),
    open(params) {
      const q = params.get("sql"), pj = params.get("p");
      if (pj != null) { paramsText = pj; params_.value = pj; params_.closest("details")?.setAttribute("open", ""); store.set("profile.params", pj); }
      if (q) { ed.value = q; sql = q; store.set("profile.sql", q); clear(out); fromLink(q, params.get("fp")); }
    },
  };
}

/** Why a linked statement asks before it runs: slow, contained, refused. */
function linkRisk(l) {
  const out = [];
  const s = l.stat;
  if (s && s.p99_ms >= 1000) out.push(`It is slow: p99 ${fmt.ms(Math.min(s.p99_ms, s.max_ms ?? s.p99_ms))} over its ${fmt.plural(s.calls, "call")} in the last hour, the slowest ${fmt.ms(s.max_ms)}.`);
  for (const r of l.rules) {
    const who = r.match?.key ? `from key ${r.match.key}` : "from every key";
    out.push(r.action === "refuse" ? `A contain rule refuses it ${who}${r.reason ? `: "${r.reason}"` : ""}.${!r.match?.key || l.refusedAll ? " Your run will be refused too (429)." : " Your key is not refused: it runs in full."}`
      : `A contain rule lets at most ${r.concurrency ?? "?"} run at once ${who}${r.reason ? `: "${r.reason}"` : ""}. Your runs take its turns.`);
  }
  return out;
}

/** The card a link opens on: the query is filled in, has not run, and what running it means. */
function linkCard(l, runs, run) {
  const s = l.stat, risk = linkRisk(l);
  const where = [clusterName(), environment()].filter(Boolean).join(", ");
  const facts = !l.fp ? null : !l.read ? h("div", { class: "small muted" }, "Reading its last hour…")
    : s ? h("div", { class: "row small", style: { gap: "4px 16px", flexWrap: "wrap" } },
      [["last hour", fmt.plural(s.calls, "call")], ["p99", fmt.ms(Math.min(s.p99_ms, s.max_ms ?? s.p99_ms))], ["slowest", fmt.ms(s.max_ms)], ["failed", fmt.n(s.errors || 0)]].map(([k, v]) => h("span", { class: "nowrap" }, h("span", { class: "muted" }, k + " "), h("b", {}, v))),
      h("a", { class: "wl-link", href: statementLink(l.fp) }, "Its statement page ›"))
    : h("div", { class: "small muted" }, "This server has no calls of it in the last hour.");
  return h("div", { class: "banner wl-linked " + (risk.length ? "warn" : ""), style: { alignItems: "flex-start" } }, icon("info"),
    h("div", { class: "stack grow", style: { gap: "6px", minWidth: 0 } },
      h("b", {}, "Filled in from a link. It has not run."),
      h("div", { class: "small dim" }, `Profile runs it ${runs === 1 ? "once" : runs + " times"} as a normal query${where ? " on " + where : ""}, under your key. Check the query and its parameters first.`),
      facts,
      risk.length ? h("ul", { class: "wl-list small" }, risk.map((x) => h("li", {}, x))) : null,
      h("div", { class: "row" }, h("button", { class: "btn primary sm", onclick: run }, icon("flame"), risk.length ? "Profile it…" : "Profile it"))));
}

// ---------------------------------------------------------------- CPU and memory

const SECONDS = [[1, "1 s"], [5, "5 s"], [10, "10 s"], [30, "30 s"]];
const HZ = [[49, "49 Hz"], [99, "99 Hz"], [499, "499 Hz"], [997, "997 Hz"]];
const EVERY = [[64 << 10, "64 KB"], [512 << 10, "512 KB"], [2 << 20, "2 MB"]];
/** An interval in bytes, as its button names it. */
const everyName = (n) => EVERY.find((e) => e[0] === n)?.[1] || fmt.bytes(n);
const CLIENTS = [[1, "1"], [4, "4"], [16, "16"]];

/** The query string that sends a profile to a node of the cluster. */
const onNode = (node) => node ? "?node=" + encodeURIComponent(node) : "";

/** What sampling costs, measured (docs/chloedb/server.md, /v1/profile): a BM25 query in a loop, 4 cores. */
const CPU_COST = { 49: "under 6%", 99: "about 6%", 499: "between 6% and 11%", 997: "about 11%" };
const HEAP_COST = { [64 << 10]: "20% to 46%", [512 << 10]: "6% to 29%", [2 << 20]: "3% to 6%" };
/** The rates and callers that need no confirming. */
const SAFE = { hz: 99, every: 2 << 20, clients: 4, seconds: 10 };

/** A copy's load in a few words: "CPU 62% · p99 18 ms · 210 q/s". */
const loadWords = (c) => [c.cpu != null ? "CPU " + fmt.pct(c.cpu, 0) : null, c.p99 != null ? "p99 " + fmt.ms(c.p99) : null, c.qps != null ? fmt.n(Math.round(c.qps)) + " q/s" : null].filter(Boolean).join(" · ") || (c.status && c.status !== "ok" ? c.status : "no load read");

/**
 * On a coordinator, which server of its cluster the tab profiles: the
 * coordinator itself, or any copy of any shard, found by its short name,
 * node or shard, with its load. The copy takes the profile itself, asked
 * through the coordinator. Hidden on any other server.
 */
function nodePicker(tab, node) {
  const el = h("div", { class: "wl-pick", style: { display: "none" } });
  const to = (url) => { location.hash = link("profile", { tab, node: url || null }); };
  let list = [], hi = 0, open = false;
  const input = h("input", { class: "input", type: "search", placeholder: "Find a copy: s117-0, a node, shard 117", "aria-label": "Find a copy to profile", autocomplete: "off", spellcheck: "false" });
  const menuEl = h("div", { class: "wl-pick-list", role: "listbox", hidden: true });
  const chip = h("div", { class: "wl-pick-cur" });
  const say = h("div", { class: "small muted" });
  function matches(q) {
    const all = copies();
    q = q.trim().toLowerCase();
    if (!q) return all.filter((c) => c.cpu != null).sort((a, b) => b.cpu - a.cpu).slice(0, 8);
    const num = /^(?:s|shard\s*)?(\d+)$/.exec(q);
    const hit = (c) => (num && String(c.shard) === num[1]) || [c.name, c.url, c.node, c.zone].some((x) => x && String(x).toLowerCase().includes(q));
    // The shard's own copies first, then by CPU.
    const own = (c) => (num && String(c.shard) === num[1]) || (c.name || "").toLowerCase() === q ? 1 : 0;
    return all.filter(hit).sort((a, b) => own(b) - own(a) || (b.cpu ?? -1) - (a.cpu ?? -1));
  }
  function drawMenu() {
    const q = input.value;
    const found = matches(q);
    list = found.slice(0, 8);
    hi = Math.min(hi, Math.max(0, list.length - 1));
    clear(menuEl,
      h("div", { class: "wl-pick-h small muted" }, q.trim() ? `${fmt.plural(found.length, "copy", "copies")} match${found.length > 8 ? ", the 8 busiest shown" : ""}` : ["The busiest copies, by CPU over the latest minute", ...((d) => d.length ? [" · ", h("span", { class: "wl-bad" }, `${d.join(", ")} down`)] : [])(copies().filter((c) => c.status === "down").map((c) => c.name))]),
      node ? h("button", { class: "wl-pick-row", role: "option", onclick: () => to(null) }, h("b", {}, "this coordinator"), h("span", { class: "small muted" }, "its own process")) : null,
      list.length ? list.map((c, i) => h("button", { class: "wl-pick-row" + (i === hi ? " hi" : ""), role: "option", "aria-selected": String(i === hi), title: copyTitle(c.url), onclick: () => to(c.url), onmouseenter: () => { hi = i; for (const [k, b] of [...menuEl.querySelectorAll(".wl-pick-row[title]")].entries()) b.classList.toggle("hi", k === i); } },
        h("span", { class: "wl-pick-name" }, h("b", { class: "mono" }, c.name), h("span", { class: "small muted" }, ` shard ${c.shard}${c.role && c.role !== "standalone" ? " · " + c.role : ""}`)),
        h("span", { class: "small " + (c.cpu != null && c.cpu > 0.85 ? "" : "muted") }, loadWords(c))))
        : h("div", { class: "wl-pick-h small muted" }, "No copy matches. Try its short name, its node or a shard number."));
    menuEl.hidden = !open;
  }
  const show = (v) => { open = v; drawMenu(); };
  input.onfocus = () => show(true);
  input.oninput = () => { hi = 0; show(true); };
  input.onkeydown = (e) => {
    if (e.key === "ArrowDown") { e.preventDefault(); hi = Math.min(list.length - 1, hi + 1); drawMenu(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); hi = Math.max(0, hi - 1); drawMenu(); }
    else if (e.key === "Enter" && list[hi]) { e.preventDefault(); to(list[hi].url); }
    else if (e.key === "Escape") { show(false); input.blur(); }
  };
  input.onblur = () => setTimeout(() => show(false), 150);
  function drawCur() {
    const c = node ? copies().find((x) => x.url === node) : null;
    clear(chip,
      h("span", { class: "small dim" }, "Profiling"),
      node ? h("a", { class: "wl-chip", href: copyLink(node), title: copyTitle(node) + " · open it in Cluster" }, h("b", { class: "mono" }, copyName(node)), c ? h("span", { class: "small muted" }, ` shard ${c.shard}${c.role && c.role !== "standalone" ? " · " + c.role : ""}`) : null)
        : h("span", { class: "wl-chip" }, h("b", {}, "this coordinator")),
      c ? h("span", { class: "small muted" }, loadWords(c)) : null,
      node ? h("button", { class: "btn sm ghost", title: "Profile the coordinator's own process", onclick: () => to(null) }, "Back to the coordinator") : null);
    clear(say, node ? (c ? "The copy takes the profile itself; the coordinator asks it over gRPC and shows what it answered." : "This URL is not among the cluster's copies now: the profile will fail with 404.") : "The coordinator's own process: it plans, fans out and merges. Find a copy to profile where the shard's work is done.");
  }
  async function start() {
    let c = state.cluster;
    if (!c) { try { c = await api("/v1/cluster"); state.cluster ||= c; } catch { return; } }
    if (c.role !== "coordinator" && !node) return;
    clear(el, h("div", { class: "row", style: { gap: "10px", flexWrap: "wrap" } }, chip, h("span", { class: "sep" }), h("div", { class: "wl-pick-box" }, input, menuEl)), say);
    el.style.display = "";
    drawCur();
  }
  const off = bus.on("cluster", () => { if (el.isConnected) { drawCur(); if (open) drawMenu(); } else off?.(); });
  start();
  return el;
}

/** What a profile at these settings costs and needs: {words, risky: [why]}. */
function costOf(key, req) {
  const risky = [];
  let words;
  if (key === "cpu") {
    words = `Sampling at ${req.hz} Hz slows the threads it interrupts: a query in a loop ran ${CPU_COST[req.hz] || "an unknown share"} fewer times a second (measured on 4 cores).`;
    if (req.hz > SAFE.hz) risky.push(`${req.hz} Hz is above ${SAFE.hz} Hz: ${CPU_COST[req.hz] || "more"} fewer queries a second while it runs, against about 6% at ${SAFE.hz} Hz.`);
  } else {
    words = `A sample every ${everyName(req.every)} walks the allocating thread's stack: a query in a loop ran ${HEAP_COST[req.every] || "an unknown share"} fewer times a second (measured on 4 cores).`;
    if (req.every < SAFE.every) risky.push(`A sample every ${everyName(req.every)} is finer than every ${everyName(SAFE.every)}: ${HEAP_COST[req.every] || "more"} fewer queries a second while it runs, against 3% to 6%.`);
  }
  if (req.clients > SAFE.clients) risky.push(`${req.clients} callers run the query at once: they take ${req.clients} query slots from the server's clients for ${req.seconds} s.`);
  if (req.seconds > SAFE.seconds && risky.length) risky.push(`It lasts ${req.seconds} s.`);
  return { words, risky };
}

function save(name, text, type) {
  const a = h("a", { href: URL.createObjectURL(new Blob([text], { type })), download: name });
  document.body.append(a); a.click();
  setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 1000);
}
const stampNow = () => new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-");

/**
 * A profile taken over a while, of the whole server or of a query run in a
 * loop: its controls, a countdown while it is taken, and `cfg.show` of
 * what it answers. cfg: {key, button, rate: {label, options, param, def,
 * title}, endpoint, doing(what, rate), empty: [title, sub], busy, show(out,
 * p, req)}.
 */
function sampler(body, cfg) {
  const k = (x) => `profile.${cfg.key}.${x}`;
  let what = store.get(k("what"), "server");
  let seconds = store.get(k("seconds"), 5);
  let rate = store.get(k("rate"), cfg.rate.def);
  let clients = store.get(k("clients"), 1);
  let sql = store.get(k("sql"), store.get("profile.cpu.sql", store.get("profile.sql", "")));
  const ed = editor({ value: sql, onChange: (v) => { sql = v; store.set(k("sql"), v); }, onRun: () => take(), catalog: cached, label: "SQL to run while profiling" });
  const qbox = h("div", { class: "stack" },
    h("div", { class: "wb-ed", style: { height: "110px", border: "1px solid var(--line)", borderRadius: "var(--r-sm)", overflow: "hidden" } }, ed.el),
    h("div", { class: "row small dim", style: { flexWrap: "wrap" } }, "Run by", seg(CLIENTS, clients, (v) => { clients = v; store.set(k("clients"), v); drawCost(); }, { title: "How many callers run it at once: 1 shows one query's work, more shows the server under load" }), "callers at once, again and again until the profile ends. Its runs are not kept in Queries > History."));
  const showQ = () => { qbox.style.display = what === "query" ? "" : "none"; };
  const go = h("button", { class: "btn primary", onclick: () => take() }, icon("flame"), cfg.button);
  const out = h("div");
  const costLine = h("div", { class: "small muted wl-cost" });
  const request = () => ({ seconds, [cfg.rate.param]: rate, ...(what === "query" ? { clients } : {}) });
  const drawCost = () => { const c = costOf(cfg.key, request()); clear(costLine, c.risky.length ? h("span", { class: "wl-warn-t" }, "Above the safe defaults: it asks first. ") : null, c.words); };
  body.append(
    card({ body: h("div", { class: "stack" },
      h("div", { class: "row", style: { flexWrap: "wrap", gap: "14px" } },
        seg([["server", "the whole server"], ["query", "a query in a loop"]], what, (v) => { what = v; store.set(k("what"), v); showQ(); drawCost(); }, { title: "Profile whatever the server is doing, or one query run again and again", class: "seg wl-segwrap" }),
        h("span", { class: "row nw small dim", style: { gap: "6px" } }, "for", seg(SECONDS, seconds, (v) => { seconds = v; store.set(k("seconds"), v); drawCost(); })),
        h("span", { class: "row small dim", style: { gap: "6px", flexWrap: "wrap" }, title: cfg.rate.title }, cfg.rate.label, seg(cfg.rate.options, rate, (v) => { rate = v; store.set(k("rate"), v); drawCost(); }, { class: "seg wl-segwrap" }))),
      qbox,
      costLine,
      h("div", { class: "row" }, go, h("span", { class: "sep" }), narrow() ? null : h("span", { class: "small muted" }, cfg.note))),
    attrs: { style: { marginBottom: "14px" } } }),
    out);
  showQ(); drawCost();
  if (!sql) tables().then(() => { if (!sql && cached()[0]) { const t = cached()[0]; const f = (t.short?.fields || []).find((x) => x.type === "text" && x.indexed !== false); ed.value = f ? `SELECT * FROM ${t.table} ORDER BY bm25(${f.name}, 'search') DESC LIMIT 10` : `SELECT * FROM ${t.table} LIMIT 10`; sql = ed.value; } }).catch(() => {});
  clear(out, empty("flame", ...cfg.empty));
  let busy = false, tick = null, gone = false;
  async function take() {
    if (busy) return;
    const req = request();
    if (what === "query") {
      const text = (ed.value || "").trim().replace(/;\s*$/, "");
      if (!text) { toast("Write a query first"); return; }
      req.sql = text;
    }
    // Above the safe defaults, it asks first, saying what it costs; on
    // production, with a reason the audit log keeps.
    const c = costOf(cfg.key, req);
    let reason = null;
    if (c.risky.length) {
      const where = cfg.node ? copyName(cfg.node) : isCoord() ? "the coordinator" : "this server";
      const ok = await confirm({ title: `Profile ${where} above the safe rates?`, ok: cfg.button,
        body: h("div", { class: "stack" }, h("ul", { class: "wl-list" }, c.risky.map((x) => h("li", {}, x))),
          h("p", { class: "small muted", style: { margin: 0 } }, `The safe defaults: ${cfg.key === "cpu" ? SAFE.hz + " Hz" : "a sample every " + everyName(SAFE.every)}, ${SAFE.clients} callers or fewer. ${isProduction() ? "This is production: say why." : ""}`)),
        reason: isProduction() ? "Why profile it this finely: kept in the audit log" : false });
      if (!ok) return;
      reason = ok.reason;
    }
    busy = true; go.disabled = true;
    const t0 = performance.now();
    const bar = h("i", { style: { width: "0%" } });
    const status = h("div", { class: "small muted" }, `${seconds} s left`);
    clear(out, card({ body: h("div", { class: "stack", style: { alignItems: "center", padding: "26px 0" } },
      h("div", { class: "small dim" }, cfg.doing(what, rate)),
      h("div", { class: "meter", style: { width: "min(420px, 90%)" } }, bar), status) }));
    tick = setInterval(() => { const el = (performance.now() - t0) / 1000; bar.style.width = Math.min(100, el / seconds * 100) + "%"; status.textContent = el < seconds ? `${Math.ceil(seconds - el)} s left` : "Reading the frames' names…"; }, 200);
    try {
      const p = await api(cfg.endpoint + onNode(cfg.node), { body: req, reason });
      if (gone) return;
      cfg.show(out, p, req);
    } catch (e) {
      if (gone) return;
      clear(out, e.status === 409 ? empty("clock", "A profile is being taken", busyWords(e.message, cfg.busy)) : e.status === 501 ? empty("info", "Not available here", e.message) : h("div", { class: "error" }, e.message));
    } finally { clearInterval(tick); busy = false; go.disabled = false; }
  }
  return { stop: () => { gone = true; clearInterval(tick); } };
}

/** A copy named by its URL, its short name (s0-1) or its node name, as its URL; anything else as given. */
function nodeUrl(v) {
  if (!v || /^https?:\/\//.test(v)) return v;
  const c = copies().find((x) => x.name === v || x.node === v);
  return c ? c.url : v;
}

// A second profile's refusal: the server names who takes the first and
// for how long still; said here with the local time it ends.
function busyWords(msg, fallback) {
  const m = /being taken, by (.+?), for (\d+) s more/.exec(msg || "");
  if (!m) return fallback;
  const who = m[1] === "open" ? "Someone" : m[1] === "root token" ? "The root token" : m[1];
  return `${who} started it. It ends in about ${m[2]} s, at ${fmt.time(Date.now() + Number(m[2]) * 1000)}. One at a time: try again then.`;
}

/** The head of a profile's answer: what it was of, downloads, the query. */
function profileHead(p, req, what, downloads) {
  return [
    h("div", { class: "row", style: { flexWrap: "wrap" } }, h("span", { class: "small muted", title: p.node ? copyTitle(p.node) : "" }, `${req.sql ? "a query in a loop" : "the whole server"}${p.node ? " on " + copyName(p.node) : ""} · ${fmt.n(p.seconds)} s ${what} · ${new Date().toLocaleTimeString()}`), h("span", { class: "sep" }),
      downloads,
      req.sql ? h("a", { class: "btn sm", href: profileLink(req.sql), title: "Its steps, once" }, icon("flame"), "Profile its steps") : null),
    req.sql ? sqlBlock(req.sql) : null,
    p.runs?.error ? h("div", { class: "error" }, "A run failed, so the loop stopped: " + p.runs.error) : null,
  ];
}

const runsKpis = (r, p) => [
  kpi("Runs", fmt.n(r.count), r.count ? `${fmt.n(Math.round(r.count / p.seconds))} a second${r.clients > 1 ? `, ${r.clients} callers` : ""}` : null),
  kpi("Median run", r.median_ms != null ? fmt.ms(r.median_ms) : "–", r.p99_ms != null ? `p99 ${fmt.ms(r.p99_ms)} · max ${fmt.ms(r.max_ms)}` : null),
];

/** The frame most of a value was in, itself: [name, value]. */
function hottest(p, value = (s) => s.samples) {
  const self = new Map();
  for (const s of p.stacks) { const f = s.frames[s.frames.length - 1]; if (f != null) self.set(f, (self.get(f) || 0) + value(s)); }
  const best = [...self].sort((x, y) => y[1] - x[1])[0];
  return best && best[1] > 0 ? [p.names[best[0]], best[1]] : null;
}

/**
 * A CPU profile: every thread of the server sampled for a while, what it
 * was running, as a flame graph and by function. Of the whole server
 * (whatever its clients send), or of one query run again and again.
 */
function cpu(body, node) {
  return sampler(body, {
    key: "cpu", button: "Take a CPU profile", endpoint: "/v1/profile/cpu", node,
    rate: { label: "at", options: HZ, param: "hz", def: 99, title: "Samples a second of each thread's CPU time: more is finer, and slows the threads it interrupts more" },
    note: "One profile at a time. The stacks are sampled with SIGPROF; frames are named from the binary's symbols.",
    doing: (what, hz) => what === "query" ? `Running the query in a loop and sampling every thread at ${hz} Hz…` : `Sampling every thread at ${hz} Hz…`,
    empty: ["Sample the server's CPU", "Take a profile of the whole server while it serves its clients, or of a query run in a loop: every thread's stacks, as a flame graph and the functions that took the most time."],
    busy: "One at a time: the timer is the process's. Try again when it ends.",
    show: showCpu,
  });
}

function showCpu(out, p, req) {
  const total = p.stacks.reduce((a, s) => a + s.samples, 0);
  // A sample stands for its share of the process's measured CPU time:
  // the system's timer delivers fewer than asked when the process is busy.
  const per = p.samples && p.cpu_seconds ? p.cpu_seconds * 1000 / p.samples : 1000 / p.hz;
  const unit = (n) => fmt.ms(n * per);
  const stamp = stampNow();
  const fg = flameGraph(p, { unit });
  const r = p.runs;
  const top = p.threads[0];
  const few = p.rate && p.rate < p.hz * 0.6;
  // Under 100 samples, a function's share is noise: no hottest function.
  const thin = total > 0 && total < 100;
  const hot = thin ? null : hottest(p);
  clear(out, h("div", { class: "stack" },
    ...profileHead(p, req, `at ${p.hz} Hz`, [
      h("button", { class: "btn sm", title: "Folded stacks (thread;root;…;leaf samples) for flamegraph.pl, inferno or speedscope", onclick: () => save(`chloedb-cpu-${stamp}.folded`, folded(p), "text/plain") }, icon("download"), "Folded"),
      h("button", { class: "btn sm", onclick: () => save(`chloedb-cpu-${stamp}.json`, JSON.stringify(p), "application/json") }, icon("download"), "JSON")]),
    h("div", { class: "grid g6 prof-kpis wl-kpis" },
      kpi("CPU time", fmt.ms(p.cpu_seconds * 1000), `of every thread, in ${fmt.n(p.seconds)} s`, "The process's CPU time (user and system) while the profile was taken, from the system"),
      kpi("Cores busy", p.cores.toFixed(2), `of ${fmt.n(p.seconds)} s`, "CPU seconds over wall seconds: the cores the process kept busy on average"),
      kpi("Samples", fmt.n(p.samples), `${fmt.n(Math.round(p.rate || 0))} a CPU second`, `${p.hz} asked for a CPU second; each sample stands for ${fmt.ms(per)} of CPU time`),
      kpi("Threads", fmt.n(p.threads.reduce((a, t) => a + t.count, 0)), top ? `${top.name} ${fmt.pct(top.samples / Math.max(1, total), 0)}` : null, "Threads sampled at least once; the busiest pool"),
      ...(r ? runsKpis(r, p) : [
        kpi("Stacks", fmt.n(p.stacks.length), `${fmt.n(p.names.length)} functions`),
        hot ? kpi("Hottest function", h("span", { class: "mono", style: { fontSize: "15px" } }, shortName(hot[0])), `${fmt.pct(hot[1] / Math.max(1, total), 1)} of the samples in itself`, hot[0]) : kpi("Hottest function", "–", thin ? "too few samples to say" : null)])),
    thin ? h("div", { class: "wl-note" }, icon("info"), h("span", {}, `Only ${fmt.plural(p.samples ?? total, "sample")}: a function's share of them is noise under about 100. Sample for longer (30 s), or while the server is busy.`)) : null,
    few ? h("div", { class: "small muted" }, `The system delivered ${fmt.n(Math.round(p.rate))} samples a CPU second of the ${p.hz} asked: its timer gives the whole process at most one a tick (250 a second on many Linux kernels). Each sample is weighed by the measured CPU time, so the times are right; fewer samples make small functions noisier, so sample for longer.`) : null,
    (p.blocked || []).includes("libc") ? h("div", { class: "small muted" }, "Samples inside the C library (malloc, memcpy, system calls) are not taken on this system: unwinding from there could deadlock before glibc 2.35. Their CPU time is spread over the rest.") : null,
    !total ? empty("info", "No samples", "No thread used the CPU while the profile was taken. Sample for longer, or while the server is busy.") : null,
    total ? card({ title: "Flame graph", sub: "each frame as wide as its CPU time, what it called under it; click one to zoom in", body: fg.graph }) : null,
    total ? card({ title: "Functions", sub: "self: CPU time in the function itself · total: with what it called · click one to find it in the graph", body: fg.functions, flush: true }) : null));
}

/**
 * Memory: the heap in use and its peak, how fast the server allocates,
 * its resident memory, over time; an allocation profile (where the bytes
 * are allocated, and which are still held); and the threads.
 */
function memory(body, node) {
  const figures = h("div", { class: "grid g6 prof-kpis wl-kpis", style: { marginBottom: "14px" } });
  const brush = (a, b) => setZoom(a == null ? null : [a, b]);
  // Another node's charts are drawn from its figures read every 2 s since
  // the tab opened; this server's from its series.
  const own = node ? { label: "memory" } : { label: "memory", domain, onBrush: brush };
  const read = [];
  const heapChart = timeChart({ height: 190, ...own, fmt: (v) => fmt.bytes(v), series: [
    { name: "heap in use", get: (p) => p.heap_bytes, area: true },
    { name: "resident", get: (p) => p.rss_bytes, color: SERIES[2] },
    { name: "heap peak", get: (p) => p.heap_peak_bytes, color: SERIES[6], dash: "4 3" },
  ] });
  const rateChart = timeChart({ height: 190, ...own, label: "allocations", fmt: (v) => fmt.bytes(v) + "/s", series: [
    { name: "allocated a second", get: (p) => p.alloc_bytes_s, area: true, color: SERIES[1] },
  ] });
  const charts = h("div", { class: "grid g2", style: { marginBottom: "14px" } },
    card({ title: "Memory", sub: node ? "the heap in use, its peak and the resident memory, since this page opened" : "the heap in use, its peak and the resident memory", body: heapChart }),
    card({ title: "Allocation rate", sub: node ? "bytes allocated a second, every thread, since this page opened" : "bytes allocated a second, every thread", body: rateChart }));
  const prof = h("div", { style: { marginBottom: "14px" } });
  const threadsBox = h("div");
  body.append(figures, charts, prof, threadsBox);
  const drawCharts = () => { const pts = node ? read : visible(); heapChart.update(pts); rateChart.update(pts); };
  drawCharts();
  const offPoints = node ? null : bus.on("points", drawCharts);
  const offRange = node ? null : bus.on("range", drawCharts);
  async function drawFigures() {
    try {
      const m = await api("/v1/profile/memory" + onNode(node));
      const hp = m.heap || {};
      let pt = last();
      if (node) {
        // The rates from two reads of the node's counts.
        const b = read[read.length - 1], secs = b && m.at_ms > b.t ? (m.at_ms - b.t) / 1000 : null;
        pt = { t: m.at_ms, heap_bytes: hp.in_use_bytes, heap_peak_bytes: hp.peak_bytes, rss_bytes: m.rss_bytes, allocated: hp.allocated_bytes, allocs: hp.allocs,
          alloc_bytes_s: secs ? Math.max(0, (hp.allocated_bytes - b.allocated) / secs) : null, allocs_s: secs ? Math.max(0, (hp.allocs - b.allocs) / secs) : null };
        read.push(pt);
        if (read.length > 900) read.shift();
        drawCharts();
      }
      clear(figures,
        kpi("Heap in use", hp.installed ? fmt.bytes(hp.in_use_bytes) : "–", hp.installed ? `peak ${fmt.bytes(hp.peak_bytes)}` : "not counted in this binary", "Bytes allocated and not yet freed, counted at every allocation and free"),
        kpi("Allocating", pt?.alloc_bytes_s != null ? fmt.bytes(pt.alloc_bytes_s) + "/s" : "–", pt?.allocs_s != null ? `${fmt.si(pt.allocs_s)} allocations a second` : null, "Over the latest 2 s"),
        kpi("Allocated", hp.installed ? fmt.bytes(hp.allocated_bytes) : "–", hp.installed ? `${fmt.si(hp.allocs)} allocations since it started` : null),
        kpi("Resident", fmt.bytes(m.rss_bytes), m.peak_rss_bytes ? `peak ${fmt.bytes(m.peak_rss_bytes)}` : null, "The process's memory in RAM: the heap, the mapped database files' pages read, the threads' stacks"),
        kpi("Query memory", m.query_memory_bytes != null ? fmt.bytes(m.query_memory_bytes) : "–", m.query_memory_bytes != null ? "held by running queries" : "no budget set"),
        kpi("Threads", fmt.n(m.threads), "in the process"));
    } catch (e) { clear(figures, failure(e, "The memory figures")); }
  }
  const stopFigures = every(2000, drawFigures);
  const heapSampler = sampler(prof, {
    key: "heap", button: "Take an allocation profile", endpoint: "/v1/profile/heap", node,
    rate: { label: "a sample every", options: EVERY, param: "every", def: 2 << 20, title: "About one allocation is sampled in every this many bytes a thread allocates: smaller is finer and slows the server more (each sample walks a stack)" },
    note: "One profile at a time. Each sampled allocation keeps its stack; one freed before the end is marked so.",
    doing: (what, every) => what === "query" ? `Running the query in a loop and sampling an allocation every ${everyName(every)}…` : `Sampling an allocation every ${everyName(every)} each thread allocates…`,
    empty: ["Where is memory allocated?", "Take an allocation profile of the whole server or of a query in a loop: the stacks that allocated the most bytes, and those still holding memory when it ends."],
    busy: "One allocation profile at a time. Try again when it ends.",
    show: showHeap,
  });
  // The threads, their CPU over the latest 2 s from two reads.
  let prev = null, byPool = store.get("profile.threads.pool", true);
  const tbl = table([
    { key: "name", label: "thread", render: (t) => h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "mono" }, t.name), t.n > 1 ? h("span", { class: "badge" }, "×" + t.n) : null) },
    { key: "tid", label: "id", num: true, render: (t) => t.n > 1 ? h("span", { class: "muted" }, "–") : String(t.tid) },
    { key: "state", label: "state", render: (t) => t.state },
    { key: "cpu_pct", label: "CPU now", num: true, sort: (t) => t.cpu_pct ?? -1, render: (t) => t.cpu_pct == null ? "–" : shareBar(Math.min(1, t.cpu_pct), t.cpu_pct > 0.8 ? "var(--crit)" : "var(--s1)") },
    { key: "cpu_s", label: "CPU time", num: true, render: (t) => fmt.ms(t.cpu_s * 1000) },
    { key: "switches", label: "waits a second", num: true, title: "Times a second it gave the CPU up: to wait on a lock, a read, a timer", sort: (t) => t.switches ?? -1, render: (t) => t.switches == null ? "–" : fmt.si(t.switches) },
    { key: "forced", label: "preempted a second", num: true, title: "Times a second the system took the CPU from it", sort: (t) => t.forced ?? -1, render: (t) => t.forced == null ? "–" : fmt.si(t.forced) },
  ], { sortKey: "cpu_pct", desc: true, compact: true, maxHeight: "460px", empty: empty("info", "No threads listed", "The system lists a process's threads on Linux.") });
  const poolSeg = seg([[true, "by pool"], [false, "each thread"]], byPool, (v) => { byPool = v; store.set("profile.threads.pool", v); drawThreads(); });
  const threadNote = h("span", { class: "small muted" });
  threadsBox.append(card({ title: "Threads", sub: "every thread of the process, from the system; CPU now over the latest 2 s", tools: h("div", { class: "row", style: { gap: "10px" } }, threadNote, poolSeg), body: tbl, flush: true }));
  let lastList = null;
  async function readThreads() {
    try {
      const j = await api("/v1/profile/threads" + onNode(node));
      const before = prev ? new Map(prev.threads.map((t) => [t.tid, t])) : null;
      const secs = prev ? Math.max(0.001, (j.at_ms - prev.at_ms) / 1000) : null;
      lastList = j.threads.map((t) => {
        const b = before?.get(t.tid);
        return { ...t, cpu_pct: b ? Math.max(0, (t.cpu_s - b.cpu_s) / secs) : null, switches: b ? Math.max(0, (t.voluntary - b.voluntary) / secs) : null, forced: b ? Math.max(0, (t.involuntary - b.involuntary) / secs) : null };
      });
      prev = j;
      drawThreads();
    } catch (e) { clear(threadNote, e.message); }
  }
  function drawThreads() {
    if (!lastList) return;
    const pool = (name) => name.replace(/[-_ #]?\d+$/, "") || name;
    let rows = lastList.map((t) => ({ ...t, n: 1 }));
    if (byPool) {
      const by = new Map();
      for (const t of lastList) {
        const k = pool(t.name);
        const g = by.get(k) || { name: k, tid: t.tid, n: 0, states: new Map(), cpu_s: 0, cpu_pct: null, switches: null, forced: null };
        g.n++; g.cpu_s += t.cpu_s;
        g.states.set(t.state, (g.states.get(t.state) || 0) + 1);
        for (const f of ["cpu_pct", "switches", "forced"]) if (t[f] != null) g[f] = (g[f] || 0) + t[f];
        by.set(k, g);
      }
      rows = [...by.values()].map((g) => ({ ...g, state: [...g.states].map(([s, n]) => (g.n > 1 ? `${n} ` : "") + s).join(", ") }));
    }
    clear(threadNote, `${fmt.plural(lastList.length, "thread")} · R running, S sleeping, D waiting on the disk`);
    tbl.update(rows);
  }
  const stopThreads = every(2000, readThreads);
  return { stop: () => { stopFigures(); stopThreads(); heapSampler.stop(); offPoints?.(); offRange?.(); } };
}

/** What an allocation profile shows, by stack: bytes allocated, bytes still in use at its end, or allocations. */
const HEAP_VALUES = [["bytes", "bytes allocated"], ["in_use", "still in use at the end"], ["allocs", "allocations"]];

function showHeap(out, p, req) {
  let value = store.get("profile.heap.value", "bytes");
  const stamp = stampNow();
  const sum = (f) => p.stacks.reduce((a, s) => a + s[f], 0);
  const est = { bytes: sum("bytes"), in_use: sum("in_use"), allocs: sum("allocs") };
  const r = p.runs;
  const graph = h("div");
  const grow = p.in_use_end - p.in_use_start;
  const valueSeg = seg(HEAP_VALUES, value, (v) => { value = v; store.set("profile.heap.value", v); draw(); });
  function draw() {
    const stacks = p.stacks.map((s) => ({ ...s, samples: s[value] })).filter((s) => s.samples > 0);
    const per = p.threads.map(() => 0);
    for (const s of stacks) per[s.thread] += s.samples;
    const view = { ...p, stacks, threads: p.threads.map((t, i) => ({ ...t, samples: per[i] })) };
    const unit = value === "allocs" ? (n) => fmt.si(n) + " allocations" : (n) => fmt.bytes(n);
    const hot = hottest(view);
    if (!view.stacks.length) { clear(graph, empty("info", value === "in_use" ? "Nothing sampled is still in use" : "No allocations sampled", value === "in_use" ? "Every sampled allocation was freed before the profile ended." : "Profile for longer, or with a smaller interval.")); return; }
    const fg = flameGraph(view, { unit });
    clear(graph, h("div", { class: "stack" },
      hot ? h("div", { class: "small muted" }, "Most, itself: ", h("span", { class: "mono" }, shortName(hot[0])), ` · ${unit(hot[1])}`) : null,
      card({ title: "Flame graph", sub: `each frame as wide as the ${HEAP_VALUES.find((x) => x[0] === value)[1]} under it; click one to zoom in`, tools: valueSeg, body: fg.graph }),
      card({ title: "Functions", sub: "self: allocated by the function itself · total: with what it called", body: fg.functions, flush: true })));
  }
  clear(out, h("div", { class: "stack" },
    ...profileHead(p, req, `a sample every ${everyName(p.every)}`, [
      h("button", { class: "btn sm", title: "Folded stacks of the bytes allocated, for flamegraph.pl, inferno or speedscope", onclick: () => save(`chloedb-heap-${stamp}.folded`, folded({ ...p, stacks: p.stacks.map((s) => ({ ...s, samples: s.bytes })) }), "text/plain") }, icon("download"), "Folded"),
      h("button", { class: "btn sm", onclick: () => save(`chloedb-heap-${stamp}.json`, JSON.stringify(p), "application/json") }, icon("download"), "JSON")]),
    h("div", { class: "grid g6 prof-kpis wl-kpis" },
      kpi("Allocated", fmt.bytes(p.allocated_bytes), `${fmt.bytes(p.allocated_bytes / Math.max(0.001, p.seconds))}/s, counted`, "Bytes every thread allocated while the profile was taken, counted at each allocation"),
      kpi("Allocations", fmt.si(p.allocs), `${fmt.si(p.allocs / Math.max(0.001, p.seconds))} a second`),
      kpi("Heap", fmt.bytes(p.in_use_end), `${grow >= 0 ? "+" : "−"}${fmt.bytes(Math.abs(grow))} while profiled`, "The heap in use at the end, and how it changed from the start"),
      kpi("Still held", fmt.bytes(est.in_use), est.bytes ? `${fmt.pct(est.in_use / est.bytes, 1)} of the sampled bytes` : null, "Of the bytes allocated while profiled, those still in use at the end (estimated from the samples)"),
      ...(r ? runsKpis(r, p) : [
        kpi("Samples", fmt.n(p.samples), p.untracked ? `${fmt.n(p.untracked)} not watched` : `${fmt.n(p.stacks.length)} stacks`, "Allocations sampled, each standing for its share of the bytes"),
        kpi("Per allocation", p.allocs ? fmt.bytes(p.allocated_bytes / p.allocs) : "–", "on average")])),
    graph));
  draw();
}

// ---------------------------------------------------------------- what is kept

/**
 * This server's runtime settings: the slow-query threshold and CPU time
 * for every query. They apply to the server this page talks to only, and
 * a restart goes back to its flags.
 */
function settingsTab(body) {
  const box = h("div", { class: "prof-set" });
  const host = location.host;
  const where = isCoord() ? "the coordinator" : "this server";
  const keep = h("div", { class: "small muted" });
  const fleet = h("div");
  let alive = true;
  body.append(
    card({ title: isCoord() ? "The coordinator's runtime settings" : "This server's runtime settings", sub: `${where} (${host}) only · set while it runs; a restart goes back to its flags`, body: h("div", { class: "stack" }, box,
      h("div", { class: "small muted" }, "A query slower than the threshold is kept as slow, with its profile. ", h("a", { class: "wl-link", href: link("queries", { tab: "history", preset: "slow" }) }, "See them in Queries > History, slow ›")),
      keep,
      isCoord() ? h("div", { class: "small muted" }, "The coordinator times a query across every shard; each copy keeps its own settings, listed below.") : null) }),
    fleet);
  /** Asks first on production, with a reason: {reason} or null when cancelled. */
  async function ask(title, what) {
    if (!isProduction()) return { reason: null };
    const ok = await confirm({ title, body: `${what} It lasts until the server restarts.`, ok: "Change it", reason: true });
    return ok ? { reason: ok.reason } : null;
  }
  const words = (b) => b.slow_ms !== undefined ? (b.slow_ms == null ? "No query will be kept as slow." : `Queries over ${fmt.ms(b.slow_ms)} will be kept as slow.`) : b.cpu_all ? "Every query will have its CPU time measured (about 0.3 µs a step)." : "Only profiled queries will have their CPU time measured.";
  async function draw() {
    try {
      const s = await api("/v1/profile/settings");
      const inp = h("input", { class: "input sm num", type: "number", min: 0, step: "any", value: s.slow_ms ?? "", placeholder: "off", id: "wl-slow-ms", style: { width: "90px" } });
      const save = async (b) => {
        const ok = await ask(`Change ${where}'s runtime settings?`, words(b));
        if (!ok) return;
        try { const r = await api("/v1/profile/settings", { method: "PUT", body: b, reason: ok.reason }); toast(r.slow_ms == null ? "No query is kept as slow" : `Queries over ${fmt.ms(r.slow_ms)} are kept as slow`, "ok"); draw(); }
        catch (e) { toast(e.message, "bad"); }
      };
      clear(box,
        h("label", { class: "small dim", for: "wl-slow-ms" }, "Keep queries slower than"),
        h("span", { class: "row nw small dim", style: { gap: "8px" } }, inp, "ms",
          h("button", { class: "btn sm primary", onclick: () => save({ slow_ms: inp.value === "" ? null : Number(inp.value) }) }, "Set"),
          h("button", { class: "btn sm", title: "Keep none as slow", onclick: () => save({ slow_ms: null }) }, "Off")),
        s.started_slow_ms !== s.slow_ms ? h("span", { class: "small muted" }, `started with ${s.started_slow_ms == null ? "none" : fmt.ms(s.started_slow_ms)}`) : null,
        h("span", { class: "sep" }),
        toggle(!!s.cpu_all, (v) => save({ cpu_all: v }), "Measure CPU time for every query (about 0.3 µs a step)"));
      // PROF-2: what survives a restart.
      keep.textContent = `To keep a threshold after a restart, start the server with --slow-ms N (this one started with ${s.started_slow_ms == null ? "none, --slow-ms 0" : `--slow-ms ${Math.round(s.started_slow_ms)}`}). Measuring every query's CPU time has no flag: it is off after a restart.`;
      if (isCoord()) drawFleet(s);
    } catch (e) { clear(box, failure(e, "This server's runtime settings")); }
  }
  /** PROF-2: every copy's settings, read 16 at a time, and one change for every server. */
  async function drawFleet(mine) {
    const cs = copies().filter((c) => c.status !== "down");
    const down = copies().length - cs.length;
    // The other coordinators too (PROF-2): set through this one.
    const coords = (state.fleet?.coordinators || []).filter((c) => !c.this && !c.gone && c.url);
    const me = (state.fleet?.coordinators || []).find((c) => c.this);
    const rows = [{ name: coords.length ? me?.name || "this coordinator" : "coordinator", title: host, self: true, ...mine, read: true },
      ...coords.map((c) => ({ name: c.name || c.url, title: c.url, coord: c.url })),
      ...cs.map((c) => ({ name: c.name, title: c.url, url: c.url, shard: c.shard }))];
    const tbody = h("tbody");
    const slowOf = (x) => x.err ? (/no profile of kind/.test(x.err) ? "older server: sets its own only" : "did not answer") : !x.read ? "reading…" : x.slow_ms == null ? "none kept" : `over ${fmt.ms(x.slow_ms)}`;
    const row = (x) => h("tr", {},
      h("td", {}, h("span", { class: "mono small", title: x.title }, x.name), x.shard != null ? h("span", { class: "tiny muted" }, ` shard ${x.shard}`) : x.coord || (x.self && coords.length) ? h("span", { class: "tiny muted" }, x.self ? " this coordinator" : " coordinator") : null),
      h("td", { class: "small", title: x.err || "" }, slowOf(x), x.read && x.started_slow_ms !== x.slow_ms ? h("span", { class: "tiny muted" }, ` · started ${x.started_slow_ms == null ? "none" : fmt.ms(x.started_slow_ms)}`) : null),
      h("td", { class: "small" }, x.read ? (x.cpu_all ? "every query" : "profiled only") : ""));
    const paint = () => clear(tbody, rows.map(row));
    const inp = h("input", { class: "input sm num", type: "number", min: 0, step: "any", placeholder: "off", value: mine.slow_ms ?? "", "aria-label": "Slow threshold for every server, ms", style: { width: "90px" } });
    const n = rows.length;
    const who = `${coords.length ? `${coords.length + 1} coordinators` : "the coordinator"} and ${fmt.plural(cs.length, "copy", "copies")}`;
    const setAll = async (b) => {
      const ok = await ask(`Change every server's runtime settings?`, `${words(b)} On ${fmt.plural(n, "server")}: ${who}.`);
      if (!ok) return;
      const failed = [];
      try { await api("/v1/profile/settings", { method: "PUT", body: b, reason: ok.reason }); } catch (e) { failed.push(`coordinator (${e.message})`); }
      await Promise.all(rows.filter((x) => x.coord).map(async (x) => {
        try { await api("/v1/profile/settings?coordinator=" + encodeURIComponent(x.coord), { method: "PUT", body: b, reason: ok.reason }); } catch (e) { failed.push(`${x.name} (${e.message})`); }
      }));
      const others = rows.filter((x) => x.url);
      for (let i = 0; i < others.length; i += 16) {
        await Promise.all(others.slice(i, i + 16).map(async (x) => {
          try { await api("/v1/profile/settings?node=" + encodeURIComponent(x.url), { method: "PUT", body: b, reason: ok.reason }); } catch (e) { failed.push(`${x.name} (${e.message})`); }
        }));
      }
      toast(failed.length ? `Set on ${n - failed.length} of ${n} servers. Not on: ${failed.slice(0, 3).join(", ")}${failed.length > 3 ? ` and ${failed.length - 3} more` : ""}` : `Set on all ${n} servers`, failed.length ? "bad" : "ok");
      draw();
    };
    const admin = can("admin", "PUT /v1/profile/settings");
    const btn = (label, b, cls = "btn sm") => { const x = h("button", { class: cls, onclick: () => setAll(b()) }, label); if (!admin) { x.disabled = true; x.title = needs("admin", "PUT /v1/profile/settings").replace(/^n/, "N"); } return x; };
    const goneN = (state.fleet?.coordinators || []).filter((c) => c.gone).length;
    clear(fleet, card({ title: "Every server's settings", sub: `${who}${down ? ` · ${down} down, not listed` : ""}${goneN ? ` · ${fmt.plural(goneN, "coordinator")} not answering, not listed` : ""} · each until it restarts`, flush: true, body: h("div", {},
      h("div", { class: "row", style: { gap: "8px", padding: "10px 12px", flexWrap: "wrap", alignItems: "center" } },
        h("span", { class: "small dim" }, "On every server: keep queries slower than"), h("span", { class: "row nw small dim", style: { gap: "8px" } }, inp, "ms",
          btn(`Set on ${n}`, () => ({ slow_ms: inp.value === "" ? null : Number(inp.value) }), "btn sm primary")),
        btn("CPU time for every query: on", () => ({ cpu_all: true })), btn("off", () => ({ cpu_all: false }))),
      h("div", { class: "tbl-wrap", style: { maxHeight: "50vh" } }, h("table", { class: "t compact wl-nums" }, h("thead", {}, h("tr", {}, ["server", "kept as slow", "CPU time measured"].map((k) => h("th", {}, k)))), tbody))) }));
    paint();
    await Promise.all(rows.filter((x) => x.coord).map(async (x) => {
      try { Object.assign(x, await api("/v1/profile/settings?coordinator=" + encodeURIComponent(x.coord)), { read: true }); } catch (e) { x.err = e.message; }
    }));
    if (!fleet.isConnected) return;
    paint();
    const others = rows.filter((x) => x.url);
    for (let i = 0; i < others.length && alive; i += 16) {
      await Promise.all(others.slice(i, i + 16).map(async (x) => {
        try { Object.assign(x, await api("/v1/profile/settings?node=" + encodeURIComponent(x.url)), { read: true }); } catch (e) { x.err = e.message; }
      }));
      if (!fleet.isConnected) return;
      paint();
    }
  }
  draw();
  return { stop: () => { alive = false; } };
}
