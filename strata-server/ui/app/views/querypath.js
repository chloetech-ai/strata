// Query path: what one search waits for. The coordinator asks every shard
// and answers when the last has; the view runs a query a number of times
// and draws each run as a radar (the coordinator in the middle, a spoke
// per shard as long as it took), the spread of each shard's times against
// the coordinator's hedge, and which shard was last how often. From the
// same runs it estimates what a lower hedge would do to the tail. Every
// time is the query's own steps (`stats.steps`), measured by the
// coordinator: a `shard` step per call, with the shard's own time inside.

import { h, s, icon, clear, fmt, card, api, toast, empty, bus, seg, store, tipOn } from "../core.js";
import { state, isCoordinator, refreshCluster } from "../data.js";

export default { id: "querypath", title: "Query path", icon: "route", group: "Monitor", keywords: "query path fan out fan-out scatter gather tail latency straggler hedge hedging shards radar slowest shard p99", render };

const quant = (xs, q) => { if (!xs.length) return null; const a = xs.slice().sort((x, y) => x - y); return a[Math.min(a.length - 1, Math.floor(q * a.length))]; };
const median = (xs) => quant(xs, 0.5);

/** One run of a query: its time, and each shard's call and own time, from its steps. */
export function parseRun(j, clientMs) {
  const steps = j?.stats?.steps || [];
  const calls = new Map(); // shard -> {call_us, own_us, start_us, replica}
  for (let k = 0; k < steps.length; k++) {
    const st = steps[k];
    if (st.op !== "shard" || st.shard == null) continue;
    const replica = (st.detail || "").split(", ").slice(1).join(", ");
    let own = null;
    for (let q = k + 1; q < steps.length && (steps[q].depth || 0) > (st.depth || 0); q++) if (steps[q].op === "on the shard") { own = steps[q].us; break; }
    // A shard asked twice (a query, then a fetch of the page's rows) adds both.
    const was = calls.get(st.shard);
    if (was) { was.call_us += st.us; was.own_us = (was.own_us || 0) + (own || 0); was.rounds++; was.end_us = Math.max(was.end_us, st.start_us + st.us); }
    else calls.set(st.shard, { shard: st.shard, start_us: st.start_us, end_us: st.start_us + st.us, call_us: st.us, own_us: own, replica, rounds: 1 });
  }
  const first = Math.min(...[...calls.values()].map((c) => c.start_us), Infinity);
  const merge = steps.filter((x) => x.op === "merge" && !(x.depth > 0)).reduce((a, x) => a + x.us, 0);
  const took = j?.stats?.took_ms ?? j?.took_ms ?? clientMs;
  return { took_ms: took, client_ms: clientMs, merge_us: merge, first_us: Number.isFinite(first) ? first : 0, calls: [...calls.values()].sort((a, b) => a.shard - b.shard), rows: j?.rows?.length ?? 0 };
}

/** The tail if each call slower than `hedge` ms had been asked again of a copy that answers in that shard's median time. */
export function hedged(runs, hedge) {
  const med = new Map();
  for (const r of runs) for (const c of r.calls) { if (!med.has(c.shard)) med.set(c.shard, []); med.get(c.shard).push(c.call_us / 1e3); }
  for (const [k, v] of med) med.set(k, median(v));
  return runs.map((r) => {
    const slowest = Math.max(0, ...r.calls.map((c) => c.call_us / 1e3));
    const after = Math.max(0, ...r.calls.map((c) => { const t = c.call_us / 1e3; return t > hedge ? Math.min(t, hedge + med.get(c.shard)) : t; }));
    return r.took_ms - slowest + after;
  });
}

function render(page) {
  page.append(h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Query path"), h("p", {}, "A search is as fast as the slowest shard it asks: run one a few times and see which shard it waited for, and what hedging buys."))));
  const form = h("div"), out = h("div");
  page.append(form, out);
  if (!isCoordinator()) {
    const wait = bus.on("health", () => { if (isCoordinator()) { wait(); clear(page); render(page); } });
    clear(form, card({ body: empty("route", "This server answers alone", "The query path shows how a coordinator asks its shards: open the command center of a coordinator (strata-server --shards …).") }));
    return wait;
  }

  let runs = [], sel = 0, running = false, stop = false, hedgeWhatIf = null, hedgesBefore = null, hedgesAfter = null;
  const sql = h("textarea", { class: "input mono", rows: 3, style: { width: "100%" }, spellcheck: "false", "aria-label": "The query to run" });
  sql.value = store.get("qpath.sql", "");
  const count = store.get("qpath.n", 20);
  let n = count;
  const nSeg = seg([[10, "10 runs"], [20, "20"], [50, "50"], [100, "100"]], n, (v) => { n = v; store.set("qpath.n", v); });
  const go = h("button", { class: "btn primary" }, icon("play"), "Run it");
  const recent = h("div", { class: "row", style: { gap: "6px" } });
  const status = h("span", { class: "small muted" });

  async function suggest() {
    const picks = [];
    try {
      const r = await api("/v1/queries/recent");
      for (const q of r.queries || r || []) { const t = (q.sql || "").trim(); if (t && /^select/i.test(t) && !picks.includes(t)) picks.push(t); if (picks.length >= 4) break; }
    } catch { /* no admin key: the table's own query */ }
    if (picks.length < 2) {
      // A search of the first table's first indexed text, for two words of its first row.
      try {
        const t = (await api("/v1/tables")).tables?.[0];
        const fields = t?.schema?.fields || [];
        const isText = (f) => f.ty && typeof f.ty === "object" && "Text" in f.ty;
        const searched = fields.find((f) => isText(f) && f.indexed), shown = fields.find((f) => isText(f) && f.stored);
        if (t && searched) {
          let words = "a";
          if (shown) {
            try {
              const r = await api("/v1/query", { body: { sql: `SELECT ${shown.name} FROM ${t.table} LIMIT 1` } });
              const v = String(r.rows?.[0]?.[0] ?? r.rows?.[0]?.[shown.name] ?? "").toLowerCase().match(/[a-z]{3,}/g);
              if (v?.length) words = v.slice(0, 2).join(" ");
            } catch { /* the plain word then */ }
          }
          picks.push(`SELECT ${t.primary_key || "*"} FROM ${t.table} ORDER BY bm25(${searched.name}, '${words.replace(/'/g, "''")}') DESC LIMIT 10`);
        } else if (t) picks.push(`SELECT * FROM ${t.table} LIMIT 10`);
      } catch { /* nothing to offer */ }
    }
    if (!sql.value && picks[0]) sql.value = picks[0];
    clear(recent, picks.length ? h("span", { class: "small muted" }, "Try:") : null, picks.map((p) => h("button", { class: "chip", title: p, onclick: () => { sql.value = p; } }, p.length > 60 ? p.slice(0, 57) + "…" : p)));
  }

  go.onclick = async () => {
    if (running) { stop = true; return; }
    const text = sql.value.trim();
    if (!text) { sql.focus(); return; }
    store.set("qpath.sql", text);
    running = true; stop = false; runs = []; sel = 0; hedgeWhatIf = null;
    clear(go, icon("stop"), "Stop");
    hedgesBefore = state.cluster?.hedges ?? null;
    try {
      for (let k = 0; k < n && !stop; k++) {
        const t0 = performance.now();
        const j = await api("/v1/query", { body: { sql: text } });
        runs.push(parseRun(j, performance.now() - t0));
        sel = runs.length - 1;
        status.textContent = `run ${runs.length} of ${n}`;
        draw();
      }
    } catch (e) {
      toast("The query: " + e.message, "bad", 7000);
    }
    running = false;
    clear(go, icon("play"), "Run it again");
    await refreshCluster();
    hedgesAfter = state.cluster?.hedges ?? null;
    status.textContent = runs.length ? `${runs.length} runs` : "";
    draw();
  };

  clear(form, card({ title: "The query", sub: "it runs one after the other, so each run sees the cluster alone", attrs: { style: { marginBottom: "14px" } },
    body: h("div", { class: "stack" }, sql, recent, h("div", { class: "row", style: { gap: "10px" } }, nSeg, go, status)) }));
  suggest();

  // ---------------------------------------------------------------- drawing
  function radar(run, shards) {
    const W = 340, R = 140, cx = W / 2, cy = W / 2;
    const svg = s("svg", { viewBox: `0 0 ${W} ${W}`, width: "100%", style: { maxWidth: "380px", display: "block", margin: "0 auto" }, role: "img", "aria-label": `Run ${sel + 1}: the coordinator waited ${fmt.ms(run.took_ms)}` });
    const slowest = Math.max(1, ...run.calls.map((c) => c.call_us));
    const scale = Math.max(slowest, ...runs.flatMap((r) => r.calls.map((c) => c.call_us)));
    for (const f of [0.25, 0.5, 0.75, 1]) svg.append(s("circle", { cx, cy, r: R * f, fill: "none", stroke: "var(--grid)" }), s("text", { x: cx + 3, y: cy - R * f + 11, style: { fontSize: "9.5px", fill: "var(--muted)" } }, fmt.ms((scale * f) / 1e3)));
    const hedge = state.cluster?.hedge_ms;
    if (hedge && hedge * 1e3 < scale) svg.append(s("circle", { cx, cy, r: (R * hedge * 1e3) / scale, fill: "none", stroke: "var(--warn)", "stroke-dasharray": "4 4" }));
    const last = run.calls.reduce((a, c) => (c.call_us > (a?.call_us ?? -1) ? c : a), null);
    run.calls.forEach((c, k) => {
      const a = (k / Math.max(1, shards)) * Math.PI * 2 - Math.PI / 2;
      const len = (R * c.call_us) / scale, own = c.own_us != null ? (R * c.own_us) / scale : null;
      const x = cx + Math.cos(a) * len, y = cy + Math.sin(a) * len;
      const isLast = c === last;
      const g = s("g", { tabindex: 0 });
      g.append(s("line", { x1: cx, y1: cy, x2: cx + Math.cos(a) * R, y2: cy + Math.sin(a) * R, stroke: "var(--line)", "stroke-width": 1 }),
        s("line", { x1: cx, y1: cy, x2: x, y2: y, stroke: isLast ? "var(--s2)" : "var(--s1)", "stroke-width": isLast ? 3.5 : 2.5, "stroke-linecap": "round" }),
        own != null ? s("line", { x1: cx + Math.cos(a) * (len - own) / 2, y1: cy + Math.sin(a) * (len - own) / 2, x2: cx + Math.cos(a) * (len + own) / 2, y2: cy + Math.sin(a) * (len + own) / 2, stroke: isLast ? "var(--s4)" : "var(--s3)", "stroke-width": 6, "stroke-linecap": "round", opacity: 0.85 }) : null,
        s("circle", { cx: x, cy: y, r: isLast ? 6 : 4.5, fill: isLast ? "var(--s2)" : "var(--s1)" }),
        s("text", { x: cx + Math.cos(a) * (R + 14), y: cy + Math.sin(a) * (R + 14) + 4, "text-anchor": "middle", style: { fontSize: "10.5px", fill: isLast ? "var(--text)" : "var(--muted)", fontWeight: isLast ? 700 : 400 } }, String(c.shard)));
      tipOn(g, () => h("div", {}, h("b", {}, `Shard ${c.shard}${isLast ? ", the last to answer" : ""}`), h("div", { class: "small" }, `the call ${fmt.ms(c.call_us / 1e3)}${c.own_us != null ? `, of which on the shard ${fmt.ms(c.own_us / 1e3)}` : ""}${c.rounds > 1 ? ` in ${c.rounds} rounds` : ""}`), c.replica ? h("div", { class: "small mono" }, c.replica) : null));
      svg.append(g);
    });
    svg.append(s("circle", { cx, cy, r: 26, fill: "var(--surface)", stroke: "var(--accent)", "stroke-width": 2 }),
      s("text", { x: cx, y: cy - 2, "text-anchor": "middle", style: { fontSize: "12px", fontWeight: 700, fill: "var(--text)" } }, fmt.ms(run.took_ms)),
      s("text", { x: cx, y: cy + 11, "text-anchor": "middle", style: { fontSize: "9px", fill: "var(--muted)" } }, "in all"));
    return svg;
  }

  function spread(shards) {
    const ids = [...new Set(runs.flatMap((r) => r.calls.map((c) => c.shard)))].sort((a, b) => a - b);
    const all = runs.flatMap((r) => r.calls.map((c) => c.call_us / 1e3));
    const hi = Math.max(1, ...all) * 1.05;
    const hedge = state.cluster?.hedge_ms;
    const W = 640, row = 18, padL = 54, H = ids.length * row + 26;
    const X = (v) => padL + (v / hi) * (W - padL - 34);
    const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, width: "100%", role: "img", "aria-label": "Each shard's call times over the runs" });
    for (const t of [0.25, 0.5, 0.75, 1]) { const x = X(hi * t); svg.append(s("line", { x1: x, y1: 0, x2: x, y2: H - 18, stroke: "var(--grid)" }), s("text", { x, y: H - 4, "text-anchor": "middle", style: { fontSize: "10px", fill: "var(--muted)" } }, fmt.ms(hi * t))); }
    if (hedge && hedge >= hi) svg.append(s("text", { x: W - 4, y: 10, "text-anchor": "end", style: { fontSize: "10px", fill: "var(--warn)" } }, `hedge ${fmt.ms(hedge)} →`));
    if (hedge && hedge < hi) svg.append(s("line", { x1: X(hedge), y1: 0, x2: X(hedge), y2: H - 18, stroke: "var(--warn)", "stroke-dasharray": "4 3" }), s("text", { x: X(hedge) + 4, y: 10, style: { fontSize: "10px", fill: "var(--warn)" } }, `hedge ${fmt.ms(hedge)}`));
    if (hedgeWhatIf != null && hedgeWhatIf < hi) svg.append(s("line", { x1: X(hedgeWhatIf), y1: 0, x2: X(hedgeWhatIf), y2: H - 18, stroke: "var(--s7)", "stroke-width": 2 }));
    ids.forEach((id, k) => {
      const y = k * row + 10;
      const xs = runs.map((r, ri) => [r.calls.find((c) => c.shard === id), ri]).filter(([c]) => c);
      const times = xs.map(([c]) => c.call_us / 1e3);
      svg.append(s("text", { x: padL - 8, y: y + 4, "text-anchor": "end", style: { fontSize: "10.5px", fill: "var(--muted)" } }, "shard " + id));
      const p50 = median(times), p99 = quant(times, 0.99);
      if (p50 != null) svg.append(s("line", { x1: X(p50), y1: y - 6, x2: X(p50), y2: y + 6, stroke: "var(--text-2)", "stroke-width": 2 }));
      if (p99 != null) svg.append(s("line", { x1: X(p99), y1: y - 6, x2: X(p99), y2: y + 6, stroke: "var(--s2)", "stroke-width": 2 }));
      for (const [c, ri] of xs) {
        const lastOfRun = runs[ri].calls.every((o) => o.call_us <= c.call_us);
        svg.append(s("circle", { cx: X(c.call_us / 1e3), cy: y, r: ri === sel ? 4.5 : 3, fill: lastOfRun ? "var(--s2)" : "var(--s1)", opacity: ri === sel ? 1 : 0.55, stroke: ri === sel ? "var(--text)" : "none" }));
      }
    });
    return h("div", {}, svg, h("div", { class: "row small muted", style: { gap: "14px" } },
      h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { style: { width: "8px", height: "8px", borderRadius: "50%", background: "var(--s2)" } }), "the run's last shard"),
      h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { style: { width: "2px", height: "12px", background: "var(--text-2)" } }), "median"),
      h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { style: { width: "2px", height: "12px", background: "var(--s2)" } }), "p99"),
      hedge ? h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { style: { width: "12px", borderTop: "2px dashed var(--warn)" } }), "where the coordinator asks another copy (--hedge-ms)") : null));
  }

  function draw() {
    if (!runs.length) { clear(out); return; }
    const run = runs[Math.min(sel, runs.length - 1)];
    const shards = state.cluster?.shards?.length || run.calls.length;
    const took = runs.map((r) => r.took_ms);
    const lastCount = new Map();
    for (const r of runs) { const l = r.calls.reduce((a, c) => (c.call_us > (a?.call_us ?? -1) ? c : a), null); if (l) lastCount.set(l.shard, (lastCount.get(l.shard) || 0) + 1); }
    const ranked = [...lastCount.entries()].sort((a, b) => b[1] - a[1]);
    const calls = runs.flatMap((r) => r.calls.map((c) => c.call_us / 1e3));
    const shardMed = median(calls), shardP99 = quant(calls, 0.99);
    const waitLast = runs.map((r) => { const ts = r.calls.map((c) => c.call_us / 1e3).sort((a, b) => a - b); return ts.length > 1 ? ts[ts.length - 1] - ts[Math.floor(ts.length / 2)] : 0; });
    const hedge = state.cluster?.hedge_ms;

    // The runs as a strip: click one to see it on the radar.
    const hi = Math.max(...took);
    const strip = h("div", { class: "qp-runs", role: "group", "aria-label": "The runs" }, runs.map((r, k) => h("button", { class: "qp-run" + (k === sel ? " sel" : ""), style: { height: Math.max(8, (r.took_ms / hi) * 54) + "px" }, title: `run ${k + 1}: ${fmt.ms(r.took_ms)}`, "aria-label": `Run ${k + 1}, ${fmt.ms(r.took_ms)}`, onclick: () => { sel = k; draw(); } })));

    const slowestCall = run.calls.reduce((a, c) => (c.call_us > (a?.call_us ?? -1) ? c : a), null);
    const net = slowestCall && slowestCall.own_us != null ? slowestCall.call_us - slowestCall.own_us : null;
    const story = slowestCall ? `Run ${sel + 1} took ${fmt.ms(run.took_ms)}. It waited for shard ${slowestCall.shard}, which took ${fmt.ms(slowestCall.call_us / 1e3)}${slowestCall.own_us != null ? ` (${fmt.ms(slowestCall.own_us / 1e3)} on the shard, ${fmt.ms(net / 1e3)} on the network)` : ""}; the median shard answered in ${fmt.ms(median(run.calls.map((c) => c.call_us / 1e3)))}.` : "No shard was asked: the query ran on the coordinator alone.";

    // What a lower hedge would have done, from these runs.
    let whatIf = null;
    if (runs.length >= 5 && run.calls.length) {
      // The delays worth trying: below the slowest call, and the one now.
      const top = Math.max(...calls);
      const opts = [hedge, ...[100, 50, 20, 10, 5, 2, 1].filter((v) => v < top && v !== hedge)].filter((v) => v != null).slice(0, 6);
      if (hedgeWhatIf == null || !opts.includes(hedgeWhatIf)) hedgeWhatIf = opts.filter((v) => v < shardP99).sort((a, b) => b - a)[0] ?? opts[0];
      const est = hedged(runs, hedgeWhatIf);
      const asked = runs.reduce((a, r) => a + r.calls.filter((c) => c.call_us / 1e3 > hedgeWhatIf).length, 0);
      whatIf = card({ title: "What a lower hedge would do", sub: "an estimate from these runs: a call slower than the hedge is asked again of a copy that answers in that shard's median time",
        body: h("div", { class: "stack" },
          h("div", { class: "row", style: { gap: "10px" } }, h("span", { class: "small muted" }, "Ask another copy after"), seg(opts.map((v) => [v, v === hedge ? `${fmt.ms(v)} (now)` : fmt.ms(v)]), hedgeWhatIf, (v) => { hedgeWhatIf = v; draw(); })),
          h("div", { class: "grid g3" },
            h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "lbl" }, "p99 now"), h("div", { class: "val" }, fmt.ms(quant(took, 0.99)))),
            h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "lbl" }, `p99 hedged after ${fmt.ms(hedgeWhatIf)}`), h("div", { class: "val" }, fmt.ms(quant(est, 0.99)))),
            h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "lbl" }, "Extra calls"), h("div", { class: "val" }, fmt.pct(asked / Math.max(1, runs.length * run.calls.length), 0)), h("div", { class: "delta" }, "of the shard calls, asked twice"))),
          h("div", { class: "small muted" }, hedge != null ? `The coordinator hedges after ${fmt.ms(hedge)} now (strata-server --hedge-ms). ${hedgesBefore != null && hedgesAfter != null ? `It hedged ${fmt.plural(Math.max(0, hedgesAfter - hedgesBefore), "time")} during these runs.` : ""}` : "")) });
    }

    clear(out,
      h("div", { class: "grid g4", style: { marginBottom: "14px" } },
        card({ body: h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "lbl" }, "The query, p50 / p99"), h("div", { class: "val" }, `${fmt.ms(median(took))}`, h("small", {}, ` / ${fmt.ms(quant(took, 0.99))}`)), h("div", { class: "delta" }, `over ${fmt.plural(runs.length, "run")}`)) }),
        card({ body: h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "lbl" }, "One shard, p50 / p99"), h("div", { class: "val" }, fmt.ms(shardMed), h("small", {}, ` / ${fmt.ms(shardP99)}`)), h("div", { class: "delta" }, `${fmt.plural(shards, "shard")} asked each time`)) }),
        card({ body: h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "lbl" }, "Waiting for the last"), h("div", { class: "val" }, fmt.ms(median(waitLast))), h("div", { class: "delta" }, "after the median shard answered")) }),
        card({ body: h("div", { class: "kpi", style: { padding: 0 } }, h("div", { class: "lbl" }, "Most often last"), h("div", { class: "val" }, ranked.length ? "shard " + ranked[0][0] : "–"), h("div", { class: "delta" }, ranked.length ? `${fmt.pct(ranked[0][1] / runs.length, 0)} of the runs${ranked[0][1] / runs.length > 2 / Math.max(2, shards) && runs.length >= 10 ? ": a slow shard, not chance" : ""}` : "")) })),
      h("div", { class: "qp-main" },
        card({ title: `Run ${sel + 1}`, sub: "a spoke per shard, as long as its call; the thick part is the shard's own time", body: h("div", { class: "stack" }, radar(run, shards), h("p", { class: "dim", style: { margin: 0 } }, story), strip) }),
        h("div", { class: "stack" },
          card({ title: "Each shard, over the runs", sub: "a dot per run; the selected run outlined", body: spread(shards) }),
          card({ title: "The last to answer", sub: "how often each shard was the one the query waited for", body: h("div", { class: "stack", style: { gap: "4px" } }, ranked.slice(0, 8).map(([id, k]) => h("div", { class: "row nw small", style: { gap: "8px" } }, h("span", { style: { width: "64px" } }, "shard " + id), h("div", { class: "grow", style: { height: "8px", background: "var(--surface-3)", borderRadius: "4px", overflow: "hidden" } }, h("div", { style: { width: (k / runs.length) * 100 + "%", height: "100%", background: "var(--s2)" } })), h("span", { class: "num", style: { width: "40px" } }, fmt.pct(k / runs.length, 0))))) }),
          whatIf)));
  }

  return () => { stop = true; };
}
