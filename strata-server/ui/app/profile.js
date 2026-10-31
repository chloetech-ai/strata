// A query's profile, drawn: the timed steps of its execution (`stats.steps`
// from the server, a tree in pre-order with each step's depth) beside its
// request's trace. A flame chart of every step on one time scale, the
// operator tree with total and self time, CPU time and rows (an EXPLAIN
// ANALYZE), the segments by the thread that searched them, where the time
// went by operator, and on a coordinator each shard's call: the network
// and the shard's own time.

import { h, s, icon, clear, fmt, card, empty, showTip, hideTip, seg, table } from "./core.js";
import { segments } from "./results.js";

// Operators by family, each family a series color.
const FAMILY = {
  search: ["bm25", "vector", "sparse", "maxsim", "column", "decay", "graph", "graphs", "ranked", "score"],
  segment: ["segment"],
  filter: ["filter", "count", "scan", "guard", "candidates"],
  rank: ["rank", "order", "exact scores", "rerank", "phase values", "cross-encoder"],
  rows: ["project", "open readers", "fetch"],
  group: ["aggregate", "facet"],
  compound: ["leaf", "join", "with", "limit", "sort", "distinct", "distinct on", "key order", "node"],
  cluster: ["shard", "on the shard", "merge", "term statistics"],
  request: ["wait for a thread", "finish", "hand back"],
};
const COLOR = { search: "var(--s1)", segment: "var(--s3)", filter: "var(--s4)", rank: "var(--s7)", rows: "var(--s5)", group: "var(--s2)", compound: "var(--s6)", cluster: "var(--s2)", request: "var(--muted)" };
const FAMILY_NAME = { search: "index search", segment: "a segment", filter: "filter and count", rank: "ranking", rows: "rows and columns", group: "groups", compound: "operators", cluster: "shards", request: "the request" };
const familyOf = (op) => Object.keys(FAMILY).find((k) => FAMILY[k].includes(op)) || "compound";
export const stepColor = (op) => COLOR[familyOf(op)];

const SPAN_COLOR = (n) => n === "admission" ? "var(--s4)" : n === "execute" ? "var(--s1)" : n === "plan" ? "var(--s5)" : n.startsWith("wait") ? "var(--s7)" : "var(--s3)";

/** The steps as a tree: [{...step, i, kids, end, self}] roots. */
export function stepTree(steps) {
  const roots = [], stack = [];
  (steps || []).forEach((st, i) => {
    const n = { ...st, depth: st.depth || 0, i, kids: [], end: st.start_us + st.us };
    while (stack.length && stack[stack.length - 1].depth >= n.depth) stack.pop();
    (stack.length ? stack[stack.length - 1].kids : roots).push(n);
    stack.push(n);
  });
  const walk = (n) => { n.kids.forEach(walk); n.self = Math.max(0, n.us - covered(n.kids)); };
  roots.forEach(walk);
  return roots;
}

/** The time a set of steps covers, overlaps counted once (helpers run at once). */
function covered(kids) {
  const iv = kids.map((k) => [k.start_us, k.start_us + k.us]).sort((a, b) => a[0] - b[0]);
  let total = 0, cur = null;
  for (const [a, b] of iv) {
    if (!cur || a > cur[1]) { if (cur) total += cur[1] - cur[0]; cur = [a, b]; }
    else cur[1] = Math.max(cur[1], b);
  }
  if (cur) total += cur[1] - cur[0];
  return total;
}

const us = (v) => fmt.ms((v || 0) / 1000);
/** `d` microseconds, as precisely as a span of `range` needs. */
const span = (d, range) => range < 20 ? d.toFixed(1) + " µs" : range < 2000 ? d.toFixed(0) + " µs" : us(d);

/** The shard each step ran on: its own, or that of the step it is inside. */
function shardsOf(steps) {
  const out = [], stack = [];
  steps.forEach((x) => {
    const d = x.depth || 0;
    while (stack.length && stack[stack.length - 1].d >= d) stack.pop();
    const sh = x.shard ?? stack[stack.length - 1]?.sh ?? null;
    out.push(sh); stack.push({ d, sh });
  });
  return out;
}
const flat = (roots) => { const out = []; const go = (n) => { out.push(n); n.kids.forEach(go); }; roots.forEach(go); return out; };

/** The figures of a profile: time, CPU, steps, segments, threads. */
export function profileFigures(p) {
  const steps = p.steps || [];
  const top = steps.filter((x) => !x.depth);
  // CPU time: each measured step's, those inside it counted in it (a
  // coordinator measures none of its own, its shards theirs).
  let cpu = null;
  const walk = (n) => { if (n.cpu_us != null) cpu = (cpu || 0) + n.cpu_us; else n.kids.forEach(walk); };
  stepTree(steps).forEach(walk);
  // The segments searched: each step's that names one, and the threads
  // their steps ran on (on a coordinator, each shard's own).
  const onSeg = steps.filter((x) => x.seg != null);
  const shard = shardsOf(steps);
  const segs = new Set(steps.map((x, i) => x.seg != null ? `${shard[i]}:${x.seg}` : null).filter(Boolean));
  const segSteps = steps.filter((x) => x.op === "segment");
  const threads = new Set((segSteps.length ? segSteps : onSeg).map((x) => x.thread || 0));
  const exec = (p.trace || []).find((x) => x.name === "execute");
  const wall = exec ? exec.dur_us : top.length ? Math.max(...top.map((x) => x.start_us + x.us)) - Math.min(...top.map((x) => x.start_us)) : 0;
  return { cpu, segs: segs.size, threads: threads.size, wall, shards: steps.filter((x) => x.op === "shard").length };
}

export function kpi(label, value, sub, title) {
  return h("div", { class: "card kpi", title }, h("div", { class: "lbl" }, label), h("div", { class: "val", style: { fontSize: "21px" } }, value), sub ? h("div", { class: "delta", style: { maxWidth: "100%" } }, sub) : null);
}

/** The whole profile view: figures, flame chart, tree, threads, hot spots, shards, plan lines. */
export function profileView(p, { compact = false } = {}) {
  const steps = p.steps || [];
  if (!steps.length && !(p.trace || []).length) return empty("flame", "No profile for this query", "Servers before profiles answer no steps; run it again on this one.");
  const roots = stepTree(steps);
  const f = profileFigures(p);
  const took = p.took_ms ?? p.ms;
  const figures = h("div", { class: "grid g6 prof-kpis" },
    kpi("Server time", fmt.ms(took), p.roundtrip_ms ? `${fmt.ms(p.roundtrip_ms)} from this page` : null, "From the request's start on the server to its answer"),
    kpi("Executing", us(f.wall), took ? `${fmt.pct(f.wall / 1000 / took, 0)} of the server's time` : null, "The executor's run (the trace's execute span)"),
    kpi("CPU", f.cpu == null ? "–" : us(f.cpu), f.cpu == null ? (p.profiled === false ? "run it with profile on" : "not measured") : f.wall ? `${(f.cpu / f.wall).toFixed(1)}× parallel` : null, "CPU time of every thread that worked for the query; above the wall time when segments ran on several threads"),
    kpi("Segments", fmt.n(f.segs), f.threads ? `on ${fmt.plural(f.threads, "thread")}` : null),
    f.shards ? kpi("Shards", fmt.n(f.shards), "calls in the profile") : kpi("Steps", fmt.n(steps.length), p.steps_dropped ? `${fmt.n(p.steps_dropped)} not kept` : null),
    kpi("Rows", fmt.n(Array.isArray(p.rows) ? p.rows.length : p.rows), [p.filter_docs != null ? `${fmt.si(p.filter_docs)} filtered` : null, p.scored ? `${fmt.si(p.scored)} scored` : null].filter(Boolean).join(" · ") || null));
  const parts = [figures];
  parts.push(card({ title: "Flame chart", sub: "every step on one time scale · click a step to zoom, hover for its figures", body: flameChart(roots, p.trace || []), tools: legend(steps) }));
  parts.push(card({ title: "Operators", sub: "an EXPLAIN ANALYZE: total time, self time (not inside a child), CPU and rows", body: opTree(roots, { bars: !compact }), flush: true }));
  const lanes = threadLanes(roots);
  const hot = hotSpots(roots);
  parts.push(h("div", { class: "grid g2" },
    card({ title: "Segments by thread", sub: "who searched which segment, when", body: lanes }),
    card({ title: "Where the time went", sub: "self time by operator", body: hot })));
  const shards = shardView(roots);
  if (shards) parts.push(card({ title: "Shards", sub: "each call: the network, and the shard's own time", body: shards }));
  if (!compact && p.plan?.length) parts.push(card({ title: "What ran", sub: "the executor's plan lines", body: segments(p.plan) }));
  return h("div", { class: "stack prof" }, parts);
}

function legend(steps) {
  const fams = [...new Set(steps.map((x) => familyOf(x.op)))];
  return h("div", { class: "row small", style: { gap: "10px" } }, fams.map((k) => h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "dot", style: { background: COLOR[k] } }), FAMILY_NAME[k])));
}

/** Lays the trace's spans in as few rows as do not overlap. */
function packSpans(spans) {
  const rows = [];
  for (const x of spans.slice().sort((a, b) => a.start_us - b.start_us)) {
    let r = rows.find((row) => row.end <= x.start_us + 0.5);
    if (!r) { r = { end: 0, items: [] }; rows.push(r); }
    r.items.push(x); r.end = x.start_us + x.dur_us;
  }
  return rows;
}

function tipOf(n) {
  return [h("div", { class: "t" }, n.op + (n.seg != null ? ` · seg ${n.seg}` : "") + (n.thread ? ` · thread ${n.thread}` : "") + (n.shard != null ? ` · shard ${n.shard}` : "")),
    n.detail ? h("div", { class: "mono", style: { marginBottom: "4px", maxWidth: "340px", overflowWrap: "anywhere" } }, n.detail) : null,
    h("div", { class: "r" }, h("span", { class: "k box", style: { background: stepColor(n.op) } }), h("b", {}, us(n.us)), h("span", { class: "n" }, "total")),
    n.self != null && n.kids?.length ? h("div", { class: "r" }, h("span", { class: "k box", style: { background: "transparent" } }), h("b", {}, us(n.self)), h("span", { class: "n" }, "self")) : null,
    n.cpu_us != null ? h("div", { class: "r" }, h("span", { class: "k box", style: { background: "transparent" } }), h("b", {}, us(n.cpu_us)), h("span", { class: "n" }, "CPU")) : null,
    n.rows != null ? h("div", { class: "r" }, h("span", { class: "k box", style: { background: "transparent" } }), h("b", {}, fmt.n(n.rows)), h("span", { class: "n" }, "rows")) : null,
    h("div", { class: "small muted", style: { marginTop: "3px" } }, `from ${us(n.start_us)}`)];
}

/**
 * The flame chart: the trace's spans on top, then a row per depth of the
 * steps, each bar as long as its step. Segments that ran at once share a
 * depth: they stack in sub-rows by thread.
 */
export function flameChart(roots, spans) {
  const all = flat(roots);
  const t0 = Math.min(0, ...all.map((n) => n.start_us), ...spans.map((x) => x.start_us));
  const t1 = Math.max(1, ...all.map((n) => n.start_us + n.us), ...spans.map((x) => x.start_us + x.dur_us));
  let dom = [t0, t1];
  const box = h("div", { class: "flame" });
  const crumb = h("div", { class: "row small", style: { marginBottom: "6px", minHeight: "24px" } });
  // Rows: each span row, then each depth (a depth with parallel steps
  // splits by thread).
  const rows = [];
  for (const r of packSpans(spans)) rows.push({ kind: "span", items: r.items });
  const base = rows.length;
  const used = place(roots, base);
  for (let r = base; r < used; r++) rows.push({ kind: "step", items: [] });
  for (const n of all) { rows[n.row].items.push(n); if (n.lane != null) rows[n.row].lane = n.lane; }
  const RH = 22, LBL = 0;
  function draw() {
    const W = widthOf(box);
    const [a, b] = dom;
    const X = (t) => LBL + (t - a) / (b - a) * (W - LBL);
    const H = rows.length * RH + 22;
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: "none", style: { width: "100%", height: H + "px" }, role: "img", "aria-label": "flame chart of the query's steps" });
    // Grid and scale.
    // Zoomed in, the ticks count from the left edge, to the precision
    // the span needs.
    const zoomed = stack.length > 0;
    for (let k = 0; k <= 5; k++) {
      const t = a + (b - a) * k / 5, x = X(t);
      const label = !zoomed || k === 0 ? us(t) : "+" + span(t - a, b - a);
      svgEl.append(s("line", { x1: x, x2: x, y1: 0, y2: H - 18, style: { stroke: "var(--grid)" } }),
        s("text", { x: Math.min(W - 2, Math.max(2, x)), y: H - 5, "text-anchor": k === 0 ? "start" : k === 5 ? "end" : "middle", style: { fill: "var(--muted)", fontSize: "10.5px" } }, label));
    }
    rows.forEach((row, ri) => {
      const y = ri * RH;
      if (row.lane != null && row.items.length) svgEl.append(s("text", { x: W - 4, y: y + 15, "text-anchor": "end", style: { fill: "var(--muted)", fontSize: "9.5px" } }, "thread " + row.lane));
      for (const it of row.items) {
        const start = it.start_us, len = row.kind === "span" ? it.dur_us : it.us;
        if (start + len < a || start > b) continue;
        const x0 = Math.max(0, X(start)), x1 = Math.min(W, X(start + len));
        const w = Math.max(1.5, x1 - x0);
        const color = row.kind === "span" ? SPAN_COLOR(it.name) : stepColor(it.op);
        const bad = row.kind === "span" && it.outcome !== "ok" && it.outcome !== "outrun";
        const g = s("g", { class: "fl-bar", tabindex: 0, style: { cursor: "pointer" } });
        g.append(s("rect", { x: x0, y: y + 2, width: w, height: RH - 4, rx: 3, style: { fill: bad ? "var(--crit)" : color, opacity: row.kind === "span" ? 0.55 : 0.88, stroke: "var(--surface)", strokeWidth: 1 } }));
        const label = row.kind === "span" ? it.name + (it.shard != null ? ` ${it.shard}` : "") : it.op === "segment" ? `seg ${it.seg}` : it.op + (it.detail && it.op !== "on the shard" ? ": " + it.detail : "");
        if (w > 40) g.append(s("text", { x: x0 + 5, y: y + 15, style: { fill: "#fff", fontSize: "11px", pointerEvents: "none" } }, clip(label, Math.floor((w - 10) / 6.4))));
        const tip = (e) => showTip(e.clientX, e.clientY, row.kind === "span" ? [h("div", { class: "t" }, it.name + (it.shard != null ? ` · shard ${it.shard}` : "")), it.replica ? h("div", { class: "mono" }, it.replica) : null, h("div", { class: "r" }, h("span", { class: "k box", style: { background: color } }), h("b", {}, us(it.dur_us)), h("span", { class: "n" }, it.outcome)), h("div", { class: "small muted" }, `from ${us(it.start_us)}`)] : tipOf(it));
        g.addEventListener("pointermove", tip);
        g.addEventListener("pointerleave", hideTip);
        g.addEventListener("click", () => { hideTip(); zoom(start, start + len, label); });
        g.addEventListener("keydown", (e) => { if (e.key === "Enter") zoom(start, start + len, label); });
        svgEl.append(g);
      }
    });
    clear(box, svgEl);
  }
  const stack = [];
  function zoom(a, b, label) {
    if (b - a < 0.5) { a -= 0.5; b += 0.5; }
    const pad = (b - a) * 0.04;
    stack.push({ dom, label });
    dom = [a - pad, b + pad];
    drawCrumb(); draw();
  }
  function drawCrumb() {
    clear(crumb, stack.length ? [h("button", { class: "btn sm", onclick: () => { dom = stack[0].dom; stack.length = 0; drawCrumb(); draw(); } }, icon("zout"), "Whole query"),
      h("span", { class: "muted" }, "zoomed to"), h("b", { class: "mono ellipsis", style: { maxWidth: "60%" } }, stack[stack.length - 1].label), h("span", { class: "muted" }, us(dom[1] - dom[0]))] : h("span", { class: "muted" }, `${us(t1 - t0)} in all · ${fmt.plural(all.length, "step")}`));
  }
  drawCrumb(); draw();
  onWidth(box, draw);
  return h("div", {}, crumb, box);
}

/**
 * Rows for steps, as a flame chart with concurrency: a step's children sit
 * in the rows under it; siblings that ran at once (segments on several
 * threads, a coordinator's calls to its shards) take bands of their own,
 * each band as deep as its deepest subtree, so that no two bars overlap.
 * Sets each step's row (and lane, the thread a band is for); returns the
 * first row free under them.
 */
function place(siblings, row) {
  const threads = new Set(siblings.map((n) => n.thread || 0));
  const bands = [];
  for (const n of siblings.slice().sort((a, b) => a.start_us - b.start_us)) {
    const t = n.thread || 0;
    let b = bands.find((x) => x.thread === t && x.end <= n.start_us + 0.05);
    if (!b) { b = { thread: t, end: 0, members: [] }; bands.push(b); }
    b.members.push(n); b.end = Math.max(b.end, n.start_us + n.us);
  }
  // Bands by thread, then as they began.
  bands.sort((a, b) => a.thread - b.thread);
  for (const b of bands) {
    let below = row + 1;
    for (const n of b.members) { n.row = row; n.lane = threads.size > 1 ? b.thread : null; below = Math.max(below, place(n.kids, row + 1)); }
    row = below;
  }
  return row;
}

// The charts draw at the width they are shown at, one unit a pixel, so that
// their text is not stretched; again when that width changes.
const widthOf = (el) => Math.max(280, el.clientWidth || 1000);
function onWidth(el, draw) {
  if (typeof ResizeObserver === "undefined") return;
  let w = el.clientWidth;
  new ResizeObserver(() => { const now = el.clientWidth; if (now && Math.abs(now - w) > 1) { w = now; draw(); } }).observe(el);
}

function clip(t, n) { return t.length > n ? t.slice(0, Math.max(1, n - 1)) + "…" : t; }

/**
 * The operator tree: a row per step, children indented under their
 * parent; runs of segments fold into one row until opened.
 */
export function opTree(roots, { bars = true } = {}) {
  const total = Math.max(1, ...roots.map((n) => n.start_us + n.us)) - Math.min(0, ...roots.map((n) => n.start_us));
  const hasCpu = flat(roots).some((n) => n.cpu_us != null);
  const tbody = h("tbody");
  const open = new Set();
  const head = h("thead", {}, h("tr", {}, h("th", {}, "step"), h("th", { class: "num" }, "total"), bars ? h("th", {}, "") : null, h("th", { class: "num", title: "time not inside a child step" }, "self"), hasCpu ? h("th", { class: "num" }, "CPU") : null, h("th", { class: "num" }, "rows"), h("th", {}, "where")));
  function rowsOf(n, depth, out) {
    out.push(tr(n, depth));
    if (n.kids.length && !n.folded) {
      // Runs of more than 6 segments fold into a summary row.
      const segs = n.kids.filter((k) => k.op === "segment");
      if (segs.length > 6 && !open.has(n.i)) {
        n.kids.filter((k) => k.op !== "segment").forEach((k) => rowsOf(k, depth + 1, out));
        out.push(segSummary(n, segs, depth + 1));
      } else n.kids.forEach((k) => rowsOf(k, depth + 1, out));
    }
    return out;
  }
  function segSummary(parent, segs, depth) {
    const t = segs.map((x) => x.us).sort((a, b) => a - b);
    const sum = t.reduce((a, b) => a + b, 0), cpu = segs.reduce((a, x) => a + (x.cpu_us || 0), 0);
    const rows = segs.reduce((a, x) => a + (x.rows || 0), 0);
    const b = h("button", { class: "btn ghost sm", onclick: () => { open.add(parent.i); draw(); } }, icon("right"), `${segs.length} segments`);
    return h("tr", { class: "prof-sum" },
      h("td", {}, h("div", { class: "row nw", style: { paddingLeft: depth * 16 + "px", gap: "6px" } }, h("span", { class: "dot", style: { background: stepColor("segment") } }), b, h("span", { class: "small muted" }, `median ${us(t[Math.floor(t.length / 2)])} · slowest ${us(t[t.length - 1])}`))),
      h("td", { class: "num" }, us(sum)), bars ? h("td", {}, h("span", { class: "small muted" }, "summed")) : null, h("td", { class: "num" }, "–"), hasCpu ? h("td", { class: "num" }, us(cpu)) : null, h("td", { class: "num" }, rows ? fmt.n(rows) : "–"), h("td", { class: "small muted" }, fmt.plural(new Set(segs.map((x) => x.thread || 0)).size, "thread")));
  }
  function tr(n, depth) {
    const toggle = n.kids.length ? h("button", { class: "btn ghost icon sm prof-tg", "aria-label": n.folded ? "Open" : "Fold", onclick: () => { n.folded = !n.folded; draw(); } }, icon(n.folded ? "right" : "down")) : h("span", { style: { width: "26px", display: "inline-block" } });
    const where = [n.seg != null ? `seg ${n.seg}` : null, n.thread ? `thread ${n.thread}` : null, n.shard != null ? `shard ${n.shard}` : null].filter(Boolean).join(" · ");
    const r = h("tr", { tabindex: 0 },
      h("td", { style: { maxWidth: "560px" } }, h("div", { class: "row nw", style: { paddingLeft: depth * 16 + "px", gap: "6px" } }, toggle, h("span", { class: "dot", style: { background: stepColor(n.op) } }), h("b", { class: "nowrap" }, n.op), n.detail ? h("span", { class: "mono small dim ellipsis", title: n.detail }, n.detail) : null)),
      h("td", { class: "num" }, h("b", {}, us(n.us))),
      bars ? h("td", {}, h("div", { class: "barbox", style: { width: "110px" } }, h("div", { class: "bar", style: { width: Math.min(100, n.us / total * 100) + "%", background: stepColor(n.op) } }))) : null,
      h("td", { class: "num" }, n.kids.length ? us(n.self) : h("span", { class: "muted" }, "–")),
      hasCpu ? h("td", { class: "num" }, n.cpu_us != null ? us(n.cpu_us) : "–") : null,
      h("td", { class: "num" }, n.rows != null ? fmt.n(n.rows) : "–"),
      h("td", { class: "small muted nowrap" }, where || ""));
    r.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, tipOf(n)));
    r.addEventListener("pointerleave", hideTip);
    return r;
  }
  function draw() { clear(tbody, roots.flatMap((n) => rowsOf(n, 0, []))); }
  draw();
  return h("div", { class: "tbl-wrap", style: { maxHeight: "520px" } }, h("table", { class: "t compact prof-tree" }, head, tbody));
}

/** The segments of each step that searched them, a lane per thread. */
export function threadLanes(roots) {
  const groups = flat(roots).filter((n) => n.kids.some((k) => k.op === "segment"));
  if (!groups.length) return h("div", { class: "small muted" }, "No segment was searched on its own: the query read an index or a cache whole.");
  const pick = h("div");
  let at = 0;
  const picker = groups.length > 1 ? seg(groups.map((g, i) => [i, `${g.op}${g.detail ? " " + clip(g.detail, 22) : ""}`]), 0, (v) => { at = v; draw(); }) : null;
  function draw() {
    const g = groups[at];
    const segs = g.kids.filter((k) => k.op === "segment");
    const lanes = [...new Set(segs.map((x) => x.thread || 0))].sort((a, b) => a - b);
    const a = Math.min(...segs.map((x) => x.start_us)), b = Math.max(...segs.map((x) => x.start_us + x.us));
    const W = widthOf(pick), RH = 24, H = lanes.length * RH + 20, L = 104;
    const X = (t) => L + (t - a) / Math.max(1e-6, b - a) * (W - L - 6);
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: "none", style: { width: "100%", height: H + "px" }, role: "img", "aria-label": "segments by thread" });
    lanes.forEach((l, i) => {
      const mine = segs.filter((x) => (x.thread || 0) === l);
      const busy = mine.reduce((acc, x) => acc + x.us, 0) / Math.max(1e-6, b - a);
      svgEl.append(s("text", { x: 4, y: i * RH + 15, style: { fill: "var(--text-2)", fontSize: "11px" } }, l === 0 ? "query thread" : "helper " + l), s("text", { x: L - 6, y: i * RH + 15, "text-anchor": "end", style: { fill: "var(--muted)", fontSize: "10px" } }, fmt.pct(Math.min(1, busy), 0)));
      for (const x of mine) {
        const r = s("rect", { x: X(x.start_us), y: i * RH + 3, width: Math.max(1.5, X(x.start_us + x.us) - X(x.start_us)), height: RH - 6, rx: 3, tabindex: 0, style: { fill: stepColor("segment"), opacity: 0.85, stroke: "var(--surface)", strokeWidth: 1, cursor: "default" } });
        r.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, tipOf(x)));
        r.addEventListener("pointerleave", hideTip);
        svgEl.append(r);
      }
    });
    svgEl.append(s("text", { x: L, y: H - 4, style: { fill: "var(--muted)", fontSize: "10.5px" } }, "0"), s("text", { x: W - 6, y: H - 4, "text-anchor": "end", style: { fill: "var(--muted)", fontSize: "10.5px" } }, us(b - a)));
    const slow = segs.slice().sort((p, q) => q.us - p.us).slice(0, 5);
    clear(pick, svgEl, h("div", { class: "small muted", style: { margin: "6px 0 4px" } }, `${fmt.plural(segs.length, "segment")} on ${fmt.plural(lanes.length, "thread")} · the busiest share of each lane at its left · slowest:`),
      h("div", { class: "row small", style: { gap: "6px" } }, slow.map((x) => h("span", { class: "badge mono", title: x.detail }, `seg ${x.seg} ${us(x.us)}`))));
  }
  draw();
  onWidth(pick, draw);
  return h("div", {}, picker ? h("div", { style: { marginBottom: "8px", overflowX: "auto" } }, picker) : null, pick);
}

/** Self time summed by operator, the most first. */
export function hotSpots(roots) {
  const by = new Map();
  for (const n of flat(roots)) by.set(n.op, (by.get(n.op) || 0) + (n.kids.length ? n.self : n.us));
  const items = [...by].sort((a, b) => b[1] - a[1]).slice(0, 9);
  const total = items.reduce((a, [, v]) => a + v, 0) || 1;
  if (!items.length) return h("div", { class: "small muted" }, "No steps.");
  return h("div", { class: "stack", style: { gap: "8px" } }, items.map(([op, v]) => h("div", {},
    h("div", { class: "row nw between small" }, h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "dot", style: { background: stepColor(op) } }), op), h("span", {}, h("b", { class: "num" }, us(v)), h("span", { class: "muted" }, " · " + fmt.pct(v / total, 0)))),
    h("div", { class: "meter", style: { marginTop: "3px" } }, h("i", { style: { width: (v / items[0][1] * 100) + "%", background: stepColor(op) } })))),
    h("div", { class: "small muted" }, "Segments searched at once count each: the sum can pass the wall time."));
}

/** Each shard call: network and the shard's own time, side by side. */
export function shardView(roots) {
  const calls = flat(roots).filter((n) => n.op === "shard");
  if (!calls.length) return null;
  const max = Math.max(...calls.map((c) => c.us), 1);
  const rows = calls.map((c) => {
    const own = c.kids.find((k) => k.op === "on the shard");
    const on = own ? own.us : 0, net = Math.max(0, c.us - on);
    const what = /^shard \d+ (\w+)/.exec(c.detail || "")?.[1] || "query";
    return { shard: c.shard, what, replica: (c.detail || "").split(", ").slice(1).join(", "), call: c.us, on, net, rows: c.rows, cpu: own?.kids.reduce((a, k) => a + (k.cpu_us || 0), 0) || null };
  });
  const bars = h("div", { class: "stack", style: { gap: "6px", marginBottom: "12px" } }, rows.map((r) => h("div", { class: "row nw", style: { gap: "10px" } },
    h("span", { class: "small nowrap", style: { minWidth: "110px" } }, h("b", {}, `shard ${r.shard}`), h("span", { class: "muted" }, " " + r.what)),
    h("div", { class: "grow", style: { display: "flex", height: "14px", borderRadius: "4px", overflow: "hidden", background: "var(--surface-3)" } },
      h("div", { style: { width: (r.net / 2 / max * 100) + "%", background: "var(--s4)" }, title: "network, half each way" }),
      h("div", { style: { width: (r.on / max * 100) + "%", background: "var(--s1)" }, title: "on the shard" }),
      h("div", { style: { width: (r.net / 2 / max * 100) + "%", background: "var(--s4)" }, title: "network, half each way" })),
    h("span", { class: "small num nowrap", style: { minWidth: "70px" } }, us(r.call)))));
  return h("div", {}, bars,
    h("div", { class: "row small", style: { gap: "12px", marginBottom: "8px" } }, h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "dot", style: { background: "var(--s4)" } }), "network (half each way)"), h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "dot", style: { background: "var(--s1)" } }), "on the shard")),
    table([
      { key: "shard", label: "shard", num: true },
      { key: "what", label: "round" },
      { key: "replica", label: "replica", render: (r) => h("span", { class: "mono small" }, r.replica) },
      { key: "call", label: "call", num: true, render: (r) => us(r.call) },
      { key: "on", label: "on the shard", num: true, render: (r) => us(r.on) },
      { key: "net", label: "network", num: true, render: (r) => r.net > r.on ? h("span", { style: { color: "var(--crit-ink)" } }, us(r.net)) : us(r.net) },
      { key: "cpu", label: "CPU", num: true, render: (r) => r.cpu ? us(r.cpu) : "–" },
      { key: "rows", label: "rows", num: true, render: (r) => fmt.n(r.rows) },
    ], { rows, sortKey: "call", desc: true, compact: true }));
}

/** One line of a profile for a list: a bar of its steps by family. */
export function miniProfile(steps) {
  const top = (steps || []).filter((x) => !x.depth);
  if (!top.length) return null;
  const total = top.reduce((a, x) => a + x.us, 0) || 1;
  return h("div", { style: { display: "flex", width: "90px", height: "8px", borderRadius: "3px", overflow: "hidden", background: "var(--surface-3)" }, title: top.map((x) => `${x.op} ${us(x.us)}`).join("\n") },
    top.map((x) => h("div", { style: { width: (x.us / total * 100) + "%", background: stepColor(x.op) } })));
}

