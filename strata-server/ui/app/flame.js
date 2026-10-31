// Flame graphs of sampled stacks (a CPU profile's, `POST /v1/profile/cpu`):
// each frame a bar as wide as the samples it was on the stack for, its
// callees under it, the heaviest first. Filter by thread, search for a
// function (its share of the samples), click a frame to zoom into it, and
// a table of the functions by self and total samples. The stacks come as
// {names, threads: [{name, count, samples}], stacks: [{thread, frames, samples}]},
// frames root first by place in names.

import { h, s, icon, clear, fmt, showTip, hideTip, debounce, table } from "./core.js";

const RH = 18;
const THREAD = -1;

/** A frame's family, for its color: whose code it is. */
function family(name) {
  if (name.startsWith("strata_server::")) return "var(--s3)";
  if (name.startsWith("strata::") || name.startsWith("<strata::")) return "var(--s1)";
  if (/^<?(tokio|hyper|h2|tonic|axum|tower|mio|futures)/.test(name)) return "var(--s4)";
  if (/^<?(std|core|alloc)::/.test(name)) return "var(--muted)";
  if (/^<?(serde|serde_json|prost)/.test(name)) return "var(--s5)";
  if (!name.includes("::")) return "var(--s6)";
  return "var(--s7)";
}

const LEGEND = [["var(--s1)", "strata"], ["var(--s3)", "strata-server"], ["var(--s4)", "async runtime, HTTP, gRPC"], ["var(--muted)", "standard library"], ["var(--s5)", "serde"], ["var(--s7)", "other crates"], ["var(--s6)", "system and C library"], ["var(--s2)", "thread"]];

/**
 * How many of a stack's first frames only start its thread (the system's
 * clone, the standard library's spawn and unwind guards): the thread's
 * name says as much. The leaf is always kept.
 */
function startOf(p, frames) {
  let i = 0;
  while (i < frames.length - 1) {
    const n = p.names[frames[i]];
    if (n.includes("::") && !/^<?(std|core|alloc)::/.test(n)) break;
    i++;
  }
  return i;
}

/** Where the `<` at `i` closes. */
function closing(name, i) {
  for (let d = 0; i < name.length; i++) {
    if (name[i] === "<") d++;
    else if (name[i] === ">" && name[i - 1] !== "-" && --d === 0) return i;
  }
  return name.length;
}

/**
 * A frame's name made short for its bar: `<T as Trait>::f` as `T::f`, no
 * generic arguments, its last two parts (three for a closure's).
 */
export function shortName(name) {
  let n = name;
  if (n.startsWith("<")) {
    const j = closing(n, 0);
    const inside = n.slice(1, j);
    let d = 0, as = -1;
    for (let i = 0; i < inside.length; i++) {
      if (inside[i] === "<") d++; else if (inside[i] === ">") d--;
      else if (d === 0 && inside.startsWith(" as ", i)) { as = i; break; }
    }
    n = (as >= 0 ? inside.slice(0, as) : inside) + n.slice(j + 1);
  }
  let out = "";
  for (let i = 0; i < n.length; i++) {
    if (n[i] === "<" && i > 0) { i = closing(n, i); continue; }
    out += n[i];
  }
  const parts = out.split("::").filter(Boolean);
  const keep = parts.length && /closure|\{\{/.test(parts[parts.length - 1]) ? 3 : 2;
  return parts.slice(-keep).join("::") || name;
}

/** The stacks as a tree: {f, v (samples), self, kids: Map}. */
function build(p, threads, byThread, trim) {
  const root = { f: null, v: 0, self: 0, kids: new Map(), depth: 0 };
  for (const st of p.stacks) {
    if (threads && !threads.has(st.thread)) continue;
    let n = root;
    n.v += st.samples;
    const frames = trim ? st.frames.slice(startOf(p, st.frames)) : st.frames;
    const path = byThread ? [THREAD - st.thread, ...frames] : frames;
    for (const f of path) {
      let k = n.kids.get(f);
      if (!k) { k = { f, v: 0, self: 0, kids: new Map(), parent: n, depth: n.depth + 1 }; n.kids.set(f, k); }
      k.v += st.samples;
      n = k;
    }
    n.self += st.samples;
  }
  return root;
}

/** Self and total samples by function, each stack counted once for a function it holds twice. */
function functions(p, threads) {
  const by = new Map();
  for (const st of p.stacks) {
    if (threads && !threads.has(st.thread)) continue;
    const seen = new Set();
    st.frames.forEach((f, i) => {
      const name = p.names[f];
      let e = by.get(name);
      if (!e) { e = { name, self: 0, total: 0 }; by.set(name, e); }
      if (!seen.has(name)) { e.total += st.samples; seen.add(name); }
      if (i === st.frames.length - 1) e.self += st.samples;
    });
  }
  return [...by.values()];
}

/** The profile as folded lines (thread;root;...;leaf samples), as the server's `?format=folded`. */
export function folded(p) {
  return p.stacks.map((st) => [p.threads[st.thread].name, ...st.frames.map((f) => p.names[f])].map((x) => x.replace(/;/g, ":")).join(";") + " " + st.samples).join("\n") + "\n";
}

/**
 * The flame graph with its tools: threads to show, a search, the graph,
 * and the functions table. `unit` names what a sample is worth
 * (`(n) => text`).
 */
export function flameGraph(p, { unit = (n) => fmt.plural(n, "sample") } = {}) {
  const total = p.stacks.reduce((a, st) => a + st.samples, 0) || 1;
  let shown = null; // null: every thread
  let byThread = true;
  let trim = true;
  let search = "";
  let zoom = null; // the node zoomed into, by its path of frames
  const chips = h("div", { class: "row", style: { gap: "6px" } });
  const find = h("input", { class: "input sm", placeholder: "Find a function…", style: { width: "220px" }, oninput: debounce(() => { search = find.value.trim().toLowerCase(); draw(); }, 150) });
  const merge = h("label", { class: "row nw small dim", style: { gap: "6px", cursor: "pointer" }, title: "One tree for every thread, or a tree under each thread's name" },
    h("input", { type: "checkbox", checked: true, onchange: (e) => { byThread = e.target.checked; zoom = null; draw(); } }), "by thread");
  const trimBox = h("label", { class: "row nw small dim", style: { gap: "6px", cursor: "pointer" }, title: "Leave out the frames that only start a thread (clone, the standard library's spawn and unwind guards)" },
    h("input", { type: "checkbox", checked: true, onchange: (e) => { trim = e.target.checked; zoom = null; draw(); } }), "hide thread starts");
  const crumb = h("div", { class: "row small", style: { minHeight: "26px", margin: "6px 0" } });
  const box = h("div", { class: "flame fg", style: { maxHeight: "620px", overflowY: "auto" } });
  const fnBox = h("div");
  const legend = h("div", { class: "row small", style: { gap: "10px", marginTop: "8px" } }, LEGEND.map(([c, l]) => h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "dot", style: { background: c } }), l)));

  const label = (n) => n.f <= THREAD ? `${p.threads[THREAD - n.f].name}` : p.names[n.f];
  const matches = (n) => search && n.f !== null && label(n).toLowerCase().includes(search);

  function drawChips() {
    clear(chips, h("button", { class: "chip", "aria-pressed": String(!shown), onclick: () => { shown = null; zoom = null; drawChips(); draw(); } }, "every thread"),
      p.threads.slice(0, 12).map((t, i) => h("button", { class: "chip", "aria-pressed": String(!!shown?.has(i)), title: `${fmt.plural(t.count, "thread")} · ${unit(t.samples)}`, onclick: () => {
        shown = shown ? new Set(shown) : new Set();
        if (shown.has(i)) shown.delete(i); else shown.add(i);
        if (!shown.size) shown = null;
        zoom = null; drawChips(); draw();
      } }, h("span", { class: "dot", style: { background: "var(--s2)" } }), `${t.name}${t.count > 1 ? " ×" + t.count : ""} ${fmt.pct(t.samples / total, 0)}`)));
  }

  function draw() {
    const root = build(p, shown, byThread, trim);
    // The zoomed node, found again by its path in the new tree.
    let z = root;
    for (const f of zoom || []) { const k = z.kids.get(f); if (!k) { zoom = null; z = root; break; } z = k; }
    const W = Math.max(320, box.clientWidth || 1000);
    const rects = [];
    // The zoomed node's ancestors, full width above it.
    const chain = [];
    for (let a = z; a && a !== root; a = a.parent) chain.unshift(a);
    chain.forEach((a, i) => rects.push({ n: a, x: 0, w: W, d: i, anc: true }));
    const base = chain.length;
    function lay(n, x, w, d) {
      if (w < 0.4) return;
      rects.push({ n, x, w, d });
      let at = x;
      for (const k of [...n.kids.values()].sort((a, b) => b.v - a.v)) {
        const kw = w * k.v / n.v;
        lay(k, at, kw, d + 1);
        at += kw;
      }
    }
    if (z === root) { let at = 0; for (const k of [...root.kids.values()].sort((a, b) => b.v - a.v)) { const kw = W * k.v / Math.max(1, root.v); lay(k, at, kw, 0); at += kw; } }
    else lay(z, 0, W, base);
    // A match inside another match counts once.
    const matched = search ? countMatched(root) : 0;
    const depth = Math.max(1, ...rects.map((r) => r.d + 1));
    const H = depth * RH + 2;
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: "none", style: { width: "100%", height: H + "px", display: "block" }, role: "img", "aria-label": "flame graph" });
    for (const r of rects) {
      const name = label(r.n);
      const hit = matches(r.n);
      const color = r.n.f <= THREAD ? "var(--s2)" : family(name);
      const g = s("g", { class: "fl-bar", tabindex: r.w > 6 ? 0 : -1, style: { cursor: "pointer" } });
      g.append(s("rect", { x: r.x, y: r.d * RH, width: Math.max(0.5, r.w - 0.5), height: RH - 1, rx: 2, style: { fill: hit ? "var(--crit)" : color, opacity: r.anc ? 0.35 : search && !hit ? 0.3 : 0.55 + (hash(name) % 40) / 100, stroke: "var(--surface)", strokeWidth: 0.5 } }));
      if (r.w > 34) g.append(s("text", { x: r.x + 4, y: r.d * RH + 13, style: { fill: "var(--text)", fontSize: "11px", pointerEvents: "none" } }, clip(r.n.f <= THREAD ? name : shortName(name), Math.floor((r.w - 8) / 6.3))));
      const tip = (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t mono", style: { overflowWrap: "anywhere" } }, name),
        h("div", { class: "r" }, h("span", { class: "k box", style: { background: color } }), h("b", {}, unit(r.n.v)), h("span", { class: "n" }, `${fmt.pct(r.n.v / total, 1)} of all`)),
        r.n.parent && r.n.parent.v ? h("div", { class: "r" }, h("span", { class: "k box", style: { background: "transparent" } }), h("b", {}, fmt.pct(r.n.v / r.n.parent.v, 1)), h("span", { class: "n" }, "of its caller")) : null,
        r.n.self ? h("div", { class: "r" }, h("span", { class: "k box", style: { background: "transparent" } }), h("b", {}, unit(r.n.self)), h("span", { class: "n" }, "in itself")) : null]);
      g.addEventListener("pointermove", tip);
      g.addEventListener("pointerleave", hideTip);
      const go = () => { hideTip(); zoom = pathOf(r.n); draw(); };
      g.addEventListener("click", go);
      g.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); });
      svgEl.append(g);
    }
    clear(box, rects.length ? svgEl : h("div", { class: "small muted", style: { padding: "12px" } }, "No samples on these threads."));
    clear(crumb, zoom ? h("button", { class: "btn sm", onclick: () => { zoom = null; draw(); } }, icon("zout"), "All") : null,
      zoom ? h("span", { class: "mono ellipsis", style: { maxWidth: "60%" } }, label(z)) : null,
      h("span", { class: "muted" }, `${unit(z.v)}${zoom ? ` · ${fmt.pct(z.v / total, 1)} of all` : shown ? ` · ${fmt.pct(root.v / total, 1)} of all, on the threads picked` : ""}`),
      search ? h("span", { class: "badge", style: { background: "var(--crit-soft, var(--surface-3))" } }, `“${search}”: ${fmt.pct(matched / Math.max(1, root.v), 1)} of the samples`) : null);
    drawFunctions();
  }

  function countMatched(root) {
    let m = 0;
    const go = (n) => { if (matches(n)) { m += n.v; return; } n.kids.forEach(go); };
    root.kids.forEach(go);
    return m;
  }

  function pathOf(n) { const out = []; for (let a = n; a && a.f !== null; a = a.parent) out.unshift(a.f); return out; }

  const fnTable = table([
    { key: "name", label: "function", render: (r) => h("span", { class: "row nw", style: { gap: "6px", maxWidth: "640px" } }, h("span", { class: "dot", style: { background: family(r.name) } }), h("span", { class: "mono small ellipsis", title: r.name }, r.name)) },
    { key: "self", label: "self", num: true, render: (r) => h("span", { class: "nowrap" }, h("b", {}, unit(r.self)), h("span", { class: "muted" }, " " + fmt.pct(r.self / total, 1))) },
    { key: "total", label: "total", num: true, render: (r) => h("span", { class: "nowrap" }, unit(r.total), h("span", { class: "muted" }, " " + fmt.pct(r.total / total, 1))) },
  ], { sortKey: "self", desc: true, max: 60, compact: true, maxHeight: "420px", onRow: (r) => { find.value = r.name; search = r.name.toLowerCase(); draw(); box.scrollIntoView({ block: "nearest" }); } });
  function drawFunctions() { fnTable.update(functions(p, shown)); }

  drawChips();
  draw();
  if (typeof ResizeObserver !== "undefined") { let w = box.clientWidth; new ResizeObserver(() => { if (box.clientWidth && Math.abs(box.clientWidth - w) > 1) { w = box.clientWidth; draw(); } }).observe(box); }
  return {
    graph: h("div", {}, h("div", { class: "row", style: { gap: "12px", flexWrap: "wrap" } }, chips, h("span", { class: "sep" }), merge, trimBox, find), crumb, box, legend),
    functions: fnTable,
  };
}

function clip(t, n) { return t.length > n ? (n > 1 ? t.slice(0, n - 1) + "…" : "") : t; }
function hash(t) { let x = 0; for (let i = 0; i < t.length; i++) x = (x * 31 + t.charCodeAt(i)) | 0; return Math.abs(x); }
