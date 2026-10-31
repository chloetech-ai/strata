// The graph explorer: rows as nodes and their links as edges, laid out by
// a force simulation that can be dragged, pinned, zoomed and panned. The
// links are an edge table's rows, an EDGES field, or a vector field's
// nearest rows. Start from a search, open a node to add its neighbours,
// rank the graph from it with personalized PageRank (ppr), or find the
// shortest path between two (PATH), each with the SQL that walks it.

import { h, s, icon, clear, fmt, api, store, empty, seg, failure, sqlBlock, debounce, showTip, hideTip, toast } from "../core.js";
import { tables, table as tableOf } from "../catalog.js";
import { SERIES } from "../charts.js";
import { quoteName } from "../sqledit.js";
import { viewport } from "./topology.js";

export default { id: "graph", title: "Graph explorer", icon: "graph", group: "Explore", full: true, keywords: "graph network nodes edges links neighbours force layout pagerank ppr path reachable hops edge table co-occurrence similarity explore", render };

const lit = (v) => typeof v === "number" ? String(v) : `'${String(v).replace(/'/g, "''")}'`;

/** Every graph the catalog holds: edge tables, EDGES fields, and nearest rows by a vector field. */
function graphs(ts) {
  const out = [];
  for (const t of ts) {
    const sh = t.short || {};
    if (sh.edges) {
      const [src, dst, w] = String(sh.edges).split(",").map((x) => x.trim());
      const over = (sh.fields || []).find((f) => f.name === src)?.references;
      if (over) out.push({ id: "t:" + t.table, kind: "table", name: t.table, nodes: over, src, dst, w, label: `${t.table} (edge table)` });
    }
    for (const f of sh.fields || []) {
      if (f.type === "edges") out.push({ id: `f:${t.table}.${f.name}`, kind: "field", name: f.name, nodes: f.references || t.table, owner: t.table, label: `${t.table}.${f.name} (EDGES)` });
      if (f.type === "vector" && !f.multi) out.push({ id: `v:${t.table}.${f.name}`, kind: "sim", name: f.name, nodes: t.table, label: `${t.table}: nearest by ${f.name}` });
    }
  }
  return out.filter((g) => tableOf(g.nodes)?.short?.key);
}

function render(page) {
  let gs = [], g = null, st = store.get("graph.state", null);
  const nodes = new Map(), edges = new Map();
  let sel = null, hover = null, pathEnds = [], ranks = new Map(), path = [], sim = null, vp = null, lastSql = "";
  const save = () => store.set("graph.state", st);
  const top = h("div", { class: "gx-top" });
  const stage = h("div", { class: "gx-stage" });
  const side = h("div", { class: "gx-side" });
  const legend = h("div", { class: "gx-legend" });
  const counts = h("div", { class: "gx-counts small muted" });
  page.append(h("div", { class: "gx" }, top, h("div", { class: "gx-main" }, h("div", { class: "gx-canvas card" }, stage, legend, counts, zoomBar()), side)));

  tables().then((ts) => {
    gs = graphs(ts);
    if (!gs.length) { clear(stage, empty("graph", "No graph yet", "An edge table (OPTIONS(edges = 'src, dst, w')), an EDGES field or a VECTOR field links rows into a graph.")); return; }
    if (!st || !gs.some((x) => x.id === st.graph)) st = fresh(gs.find((x) => x.kind === "table") || gs.find((x) => x.kind === "field") || gs[0]);
    g = gs.find((x) => x.id === st.graph);
    save(); drawTop(); setup(); start();
  }).catch((e) => clear(stage, failure(e, "The tables")));

  function fresh(gr) {
    const nt = tableOf(gr.nodes);
    const fs = nt.short.fields || [];
    return { graph: gr.id, dir: "both", color: fs.find((f) => f.type === "keyword" && !f.multi && f.name !== nt.short.key && !f.references)?.name || "", size: fs.find((f) => (f.type === "int" || f.type === "float") && /^(n|sold|count|views|plays|clicks|degree|popularity)$/i.test(f.name))?.name || "", label: fs.find((f) => f.type === "text" && f.stored !== false)?.name || nt.short.key, q: "" };
  }
  const nt = () => tableOf(g.nodes);
  const key = () => nt().short.key;
  const G = () => quoteName(g.name);

  // ------------------------------------------------------------ controls
  function drawTop() {
    const fs = nt().short.fields || [];
    const q = h("input", { class: "input rl-q", style: { fontSize: "14px" }, value: st.q, placeholder: `Add nodes: search ${g.nodes}${fs.some((f) => f.type === "text" && f.indexed !== false) ? " by words" : " by key"}…`, onkeydown: (e) => { if (e.key === "Enter") { st.q = e.target.value; save(); seed(st.q); } } });
    clear(top,
      h("label", { class: "rx-lbl" }, "graph", h("select", { class: "select sm", onchange: (e) => { st = fresh(gs.find((x) => x.id === e.target.value)); g = gs.find((x) => x.id === st.graph); save(); drawTop(); reset(); start(); } }, gs.map((x) => h("option", { value: x.id, selected: x.id === g.id }, x.label)))),
      g.kind !== "sim" ? h("label", { class: "rx-lbl" }, "follow", seg([["out", "out"], ["in", "in"], ["both", "both"]], st.dir, (v) => { st.dir = v; save(); })) : null,
      h("label", { class: "rx-lbl" }, "color", h("select", { class: "select sm mono", onchange: (e) => { st.color = e.target.value; save(); refetch(); } }, h("option", { value: "" }, "(none)"), fs.filter((f) => f.type === "keyword" && !f.multi && f.name !== key()).map((f) => h("option", { value: f.name, selected: f.name === st.color }, f.name)))),
      h("label", { class: "rx-lbl" }, "size", h("select", { class: "select sm mono", onchange: (e) => { st.size = e.target.value; save(); refetch(); } }, h("option", { value: "" }, "(links)"), fs.filter((f) => f.type === "int" || f.type === "float").map((f) => h("option", { value: f.name, selected: f.name === st.size }, f.name)))),
      h("span", { class: "rl-qwrap" }, icon("search"), q),
      h("button", { class: "btn sm", title: "Add the neighbours of every node with one link or none", onclick: expandLeaves }, icon("plus"), "Grow"),
      h("button", { class: "btn sm ghost", title: "Start the layout again", onclick: () => { for (const n of nodes.values()) n.fixed = false; kick(1); } }, icon("refresh"), "Re-layout"),
      h("button", { class: "btn sm ghost", onclick: () => { reset(); drawSide(); } }, icon("trash"), "Clear"));
  }
  function zoomBar() {
    return h("div", { class: "gx-zoom" },
      h("button", { class: "btn icon sm", title: "Zoom in", onclick: () => vp?.zoom(1.25) }, icon("zin")),
      h("button", { class: "btn icon sm", title: "Zoom out", onclick: () => vp?.zoom(0.8) }, icon("zout")),
      h("button", { class: "btn icon sm", title: "Fit", onclick: () => vp?.fit(40) }, icon("fit")));
  }

  // ------------------------------------------------------------ data
  const nodeCols = () => [...new Set([key(), st.label, st.color, st.size].filter(Boolean))];
  async function rowsOf(keys) {
    const want = keys.filter((k) => k != null);
    if (!want.length) return [];
    const cols = nodeCols();
    const out = [];
    for (let i = 0; i < want.length; i += 200) {
      const j = await api("/v1/sql", { body: { sql: `SELECT ${cols.map(quoteName).join(", ")} FROM ${quoteName(g.nodes)} WHERE ${quoteName(key())} IN (${want.slice(i, i + 200).map(lit).join(", ")}) LIMIT 200` } });
      for (const r of j.rows) out.push(Object.fromEntries(cols.map((c, k) => [c, r[k]])));
    }
    return out;
  }
  function addNode(row, near) {
    const id = String(row[key()]);
    let n = nodes.get(id);
    if (!n) {
      const a = Math.random() * Math.PI * 2, d = near ? 40 + Math.random() * 30 : 120 * Math.random();
      n = { id, x: (near?.x ?? 0) + Math.cos(a) * d, y: (near?.y ?? 0) + Math.sin(a) * d, vx: 0, vy: 0, fixed: false, expanded: false };
      nodes.set(id, n);
    }
    n.row = row;
    return n;
  }
  function addEdge(a, b, w, directed = true) {
    if (a === b) return;
    const k = directed ? `${a}>${b}` : [a, b].sort().join("~");
    const e = edges.get(k) || { s: a, t: b, w: 0, directed };
    e.w = Math.max(e.w, w ?? 1);
    edges.set(k, e);
  }

  /** The neighbours of a node, by the graph's kind; adds them and their links. */
  async function expand(id, limit = 24) {
    const n = nodes.get(id);
    let pairs = [];
    if (g.kind === "table") {
      const E = quoteName(g.name), S = quoteName(g.src), D = quoteName(g.dst), W = g.w ? quoteName(g.w) : null;
      const q = (col) => `SELECT ${S}, ${D}${W ? ", " + W : ""} FROM ${E} WHERE ${col} = ${lit(id)}${W ? ` ORDER BY ${W} DESC` : ""} LIMIT ${limit}`;
      const sqls = (st.dir === "out" ? [q(S)] : st.dir === "in" ? [q(D)] : [q(S), q(D)]);
      lastSql = sqls.join(";\n");
      for (const sql of sqls) { const j = await api("/v1/sql", { body: { sql } }); for (const r of j.rows) pairs.push([String(r[0]), String(r[1]), W ? r[2] : 1]); }
    } else if (g.kind === "field") {
      const dirs = st.dir === "both" ? ["out", "in"] : [st.dir];
      const sqls = dirs.map((d) => `SELECT ${quoteName(key())}, hops() FROM ${quoteName(g.nodes)} WHERE REACHABLE(${quoteName(g.name)}, ${lit(id)}, hops => 1, dir => '${d}') LIMIT ${limit + 1}`);
      lastSql = sqls.join(";\n");
      for (const [i, sql] of sqls.entries()) { const j = await api("/v1/sql", { body: { sql } }); for (const r of j.rows) { const o = String(r[0]); if (o !== id) pairs.push(dirs[i] === "out" ? [id, o, 1] : [o, id, 1]); } }
    } else {
      const sql = `SELECT ${quoteName(key())}, score() FROM ${quoteName(g.nodes)} ORDER BY similarity(${quoteName(g.name)}, :v) DESC LIMIT 7`;
      lastSql = sql + `\n-- :v = {"vector_of": ${JSON.stringify(id)}, "field": "${g.name}", "table": "${g.nodes}"}`;
      const j = await api("/v1/query", { body: { sql, params: { v: { vector_of: id, field: g.name, table: g.nodes } } } });
      for (const r of j.rows) if (String(r[0]) !== id) pairs.push([id, String(r[0]), r[1]]);
    }
    const others = [...new Set(pairs.flatMap(([a, b]) => [a, b]).filter((k) => !nodes.has(k)))];
    for (const row of await rowsOf(others)) addNode(row, n);
    for (const [a, b, w] of pairs) if (nodes.has(a) && nodes.has(b)) addEdge(a, b, w, g.kind !== "sim");
    await linksAmong(others);
    if (n) n.expanded = true;
    rebuild(); kick(0.6);
    return pairs.length;
  }

  /** The edge table's links between new nodes and the rest, so the picture is the graph the nodes induce, not only the walks that found them. */
  async function linksAmong(fresh) {
    if (g.kind !== "table" || !fresh.length || nodes.size > 400) return;
    const all = [...nodes.keys()].map(lit).join(", "), now = fresh.map(lit).join(", ");
    const E = quoteName(g.name), S = quoteName(g.src), D = quoteName(g.dst), W = g.w ? quoteName(g.w) : null;
    const sql = `SELECT ${S}, ${D}${W ? ", " + W : ""} FROM ${E} WHERE (${S} IN (${now}) AND ${D} IN (${all})) OR (${D} IN (${now}) AND ${S} IN (${all})) LIMIT 2000`;
    try { const j = await api("/v1/sql", { body: { sql } }); for (const r of j.rows) addEdge(String(r[0]), String(r[1]), W ? r[2] : 1); } catch { /* a picture without them */ }
  }

  async function seed(q) {
    const t = nt(), text = (t.short.fields || []).find((f) => f.type === "text" && f.indexed !== false);
    try {
      const sql = text && q.trim() ? `SELECT ${nodeCols().map(quoteName).join(", ")} FROM ${quoteName(g.nodes)} WHERE MATCH(${quoteName(text.name)}, :q) ORDER BY bm25(${quoteName(text.name)}, :q) DESC LIMIT 3`
        : `SELECT ${nodeCols().map(quoteName).join(", ")} FROM ${quoteName(g.nodes)} WHERE ${quoteName(key())} = :q LIMIT 1`;
      const j = await api("/v1/query", { body: { sql, params: { q: q.trim() } } });
      const cols = j.columns;
      if (!j.rows.length) { toast("Nothing found for " + q, "warn"); return; }
      const added = j.rows.map((r) => addNode(Object.fromEntries(cols.map((c, i) => [c, r[i]]))));
      rebuild();
      for (const n of added) await expand(n.id, 12);
      select(added[0].id);
    } catch (e) { toast(e.message, "bad"); }
  }

  /** Starts from the busiest nodes: the most linked keys of an edge table, else a search's hits. */
  async function start() {
    try {
      if (g.kind === "table") {
        const j = await api("/v1/sql", { body: { sql: `SELECT ${quoteName(g.src)}, COUNT(*) AS n FROM ${quoteName(g.name)} GROUP BY ${quoteName(g.src)} ORDER BY n DESC LIMIT 2` } });
        for (const row of await rowsOf(j.rows.map((r) => String(r[0])))) addNode(row);
      } else {
        const j = await api("/v1/sql", { body: { sql: `SELECT ${nodeCols().map(quoteName).join(", ")} FROM ${quoteName(g.nodes)} LIMIT 2` } });
        for (const r of j.rows) addNode(Object.fromEntries(j.columns.map((c, i) => [c, r[i]])));
      }
      rebuild();
      for (const n of [...nodes.values()]) await expand(n.id, 14);
      await expandLeaves(6);
      vp?.fit(40); settle = true; drawSide();
    } catch (e) { clear(stage, failure(e, "The graph")); }
  }
  async function expandLeaves(limit = 8) {
    const deg = degrees();
    const leaves = [...nodes.values()].filter((n) => !n.expanded && (deg.get(n.id) || 0) <= 1).slice(0, 14);
    for (const n of leaves) await expand(n.id, limit);
  }
  async function refetch() {
    for (const row of await rowsOf([...nodes.keys()])) nodes.get(String(row[key()])).row = row;
    rebuild(); drawSide();
  }
  function reset() { nodes.clear(); edges.clear(); sel = null; ranks = new Map(); path = []; pathEnds = []; rebuild(); }
  function degrees() { const d = new Map(); for (const e of edges.values()) { d.set(e.s, (d.get(e.s) || 0) + 1); d.set(e.t, (d.get(e.t) || 0) + 1); } return d; }

  // ------------------------------------------------------------ drawing
  let svgEl, root, gE, gN, els = new Map(), eEls = new Map(), palette = new Map();
  function setup() {
    svgEl = s("svg", { class: "gx-svg" });
    root = s("g");
    gE = s("g", { class: "gx-edges" }); gN = s("g", { class: "gx-nodes" });
    const defs = s("defs", {}, s("marker", { id: "gx-arrow", viewBox: "0 0 10 10", refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, markerUnits: "userSpaceOnUse", orient: "auto-start-reverse" }, s("path", { d: "M0 0L10 5L0 10z", class: "gx-arrowhead" })));
    svgEl.append(defs, s("rect", { class: "bg", x: -5000, y: -5000, width: 10000, height: 10000, fill: "transparent" }), root);
    root.append(gE, gN);
    clear(stage, svgEl);
    vp = viewport(svgEl, root);
    const r = stage.getBoundingClientRect();
    vp.state = { k: 1, x: (r.width || 900) / 2, y: (r.height || 600) / 2 };
    svgEl.addEventListener("click", (e) => { if (e.target === svgEl || e.target.classList.contains("bg")) { select(null); } });
  }
  const colorOf = (n) => {
    if (!st.color) return SERIES[0];
    const v = String(n.row?.[st.color] ?? "");
    if (!palette.has(v)) palette.set(v, palette.size < 8 ? SERIES[palette.size] : "var(--muted)");
    return palette.get(v);
  };
  function radius(n, deg) {
    if (st.size) {
      const vals = [...nodes.values()].map((x) => Number(x.row?.[st.size]) || 0);
      const max = Math.max(1, ...vals), v = Number(n.row?.[st.size]) || 0;
      return 5 + 13 * Math.sqrt(Math.max(0, v) / max);
    }
    return 5 + Math.min(12, Math.sqrt(deg.get(n.id) || 0) * 3);
  }
  function rebuild() {
    if (!gN) return;
    const deg = degrees();
    palette = new Map(palette);
    // edges
    for (const [k, el] of eEls) if (!edges.has(k)) { el.remove(); eEls.delete(k); }
    const maxW = Math.max(1e-9, ...[...edges.values()].map((e) => e.w || 0));
    for (const [k, e] of edges) {
      let el = eEls.get(k);
      if (!el) { el = s("line", { class: "gx-edge", "marker-end": e.directed && g.kind !== "sim" ? "url(#gx-arrow)" : null }); gE.append(el); eEls.set(k, el); }
      el.style.strokeWidth = (0.8 + 2.6 * (e.w || 0) / maxW).toFixed(2);
      el.edge = e;
    }
    // nodes
    for (const [id, el] of els) if (!nodes.has(id)) { el.remove(); els.delete(id); }
    for (const n of nodes.values()) {
      let el = els.get(n.id);
      if (!el) {
        el = s("g", { class: "gx-node", tabindex: 0, "data-key": n.id });
        el.append(s("circle", { class: "gx-halo" }), s("circle", { class: "gx-dot" }), s("text", { class: "gx-lbl", y: 4 }), s("text", { class: "gx-rank", y: 3.5, "text-anchor": "middle" }));
        dragNode(el, n);
        el.addEventListener("pointerenter", (e) => { hover = n.id; focusHover(); tip(e, n); });
        el.addEventListener("pointermove", (e) => tip(e, n));
        el.addEventListener("pointerleave", () => { hover = null; focusHover(); hideTip(); });
        el.addEventListener("click", (e) => { if (el.moved) return; if (e.shiftKey && sel && sel !== n.id) findPath(sel, n.id); else select(n.id); });
        el.addEventListener("dblclick", () => { n.fixed = false; kick(0.3); });
        el.addEventListener("keydown", (e) => { if (e.key === "Enter") select(n.id); });
        gN.append(el); els.set(n.id, el);
      }
      const r = radius(n, deg);
      n.r = r;
      const [halo, dot, lbl, rk] = el.children;
      halo.setAttribute("r", r + 5); dot.setAttribute("r", r); dot.style.fill = colorOf(n);
      lbl.setAttribute("x", r + 5);
      const text = String(n.row?.[st.label] ?? n.id);
      lbl.textContent = text.length > 34 ? text.slice(0, 32) + "…" : text;
      el.setAttribute("aria-label", text);
      rk.textContent = ranks.has(n.id) ? ranks.get(n.id) + 1 : "";
      el.classList.toggle("sel", n.id === sel);
      el.classList.toggle("ranked", ranks.has(n.id));
      el.classList.toggle("onpath", path.includes(n.id));
      el.classList.toggle("big", (deg.get(n.id) || 0) >= 5 || n.id === sel || (ranks.get(n.id) ?? 99) < 5 || path.includes(n.id));
      el.classList.toggle("pinned", n.fixed);
    }
    for (const id of [...path, sel]) if (id && els.has(id)) gN.append(els.get(id));
    for (const [k, el] of eEls) { const e = edges.get(k); const i = path.indexOf(e.s), j = path.indexOf(e.t); el.classList.toggle("onpath", i >= 0 && j >= 0 && Math.abs(i - j) === 1); }
    clear(counts, `${fmt.plural(nodes.size, "node")} · ${fmt.plural(edges.size, "link")}`, g ? ` · ${g.label}` : "");
    drawLegend(); place(); focusHover();
  }
  function drawLegend() {
    if (!st.color || !palette.size) { clear(legend); return; }
    clear(legend, [...palette.entries()].slice(0, 9).map(([v, c]) => h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "gx-ldot", style: { background: c } }), v || "(none)")));
  }
  function place() {
    for (const n of nodes.values()) els.get(n.id)?.setAttribute("transform", `translate(${n.x.toFixed(1)},${n.y.toFixed(1)})`);
    for (const el of eEls.values()) {
      const e = el.edge, a = nodes.get(e.s), b = nodes.get(e.t);
      if (!a || !b) continue;
      const dx = b.x - a.x, dy = b.y - a.y, d = Math.hypot(dx, dy) || 1;
      const ra = a.r || 6, rb = (b.r || 6) + (e.directed ? 3 : 0);
      el.setAttribute("x1", (a.x + dx / d * ra).toFixed(1)); el.setAttribute("y1", (a.y + dy / d * ra).toFixed(1));
      el.setAttribute("x2", (b.x - dx / d * rb).toFixed(1)); el.setAttribute("y2", (b.y - dy / d * rb).toFixed(1));
    }
  }
  function focusHover() {
    const f = hover || null;
    const near = new Set(f ? [f] : []);
    if (f) for (const e of edges.values()) { if (e.s === f) near.add(e.t); if (e.t === f) near.add(e.s); }
    svgEl?.classList.toggle("focus", !!f);
    for (const [id, el] of els) el.classList.toggle("near", near.has(id));
    for (const el of eEls.values()) el.classList.toggle("near", !!f && (el.edge.s === f || el.edge.t === f));
  }
  function tip(e, n) {
    const deg = degrees().get(n.id) || 0;
    showTip(e.clientX, e.clientY, [h("div", { class: "t mono" }, n.id), h("div", {}, String(n.row?.[st.label] ?? "")),
      st.color ? h("div", { class: "r" }, h("span", { class: "k box", style: { background: colorOf(n) } }), h("span", { class: "n", style: { marginLeft: 0 } }, String(n.row?.[st.color] ?? ""))) : null,
      h("div", { class: "r" }, h("b", {}, deg), h("span", { class: "n" }, `link${deg === 1 ? "" : "s"} shown${n.expanded ? "" : " · click to open"}`)),
      ranks.has(n.id) ? h("div", { class: "r" }, h("b", {}, "#" + (ranks.get(n.id) + 1)), h("span", { class: "n" }, "by PageRank from the selected node")) : null]);
  }
  function dragNode(el, n) {
    el.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      e.stopPropagation();
      let lx = e.clientX, ly = e.clientY, moved = 0;
      el.setPointerCapture(e.pointerId);
      const move = (ev) => { const dx = (ev.clientX - lx) / vp.k, dy = (ev.clientY - ly) / vp.k; lx = ev.clientX; ly = ev.clientY; moved += Math.abs(dx) + Math.abs(dy); if (moved > 3) { n.fixed = true; n.x += dx; n.y += dy; n.vx = n.vy = 0; el.classList.add("pinned"); place(); kick(0.15); } };
      const up = () => { el.removeEventListener("pointermove", move); el.removeEventListener("pointerup", up); el.moved = moved > 3; setTimeout(() => (el.moved = false), 0); };
      el.addEventListener("pointermove", move); el.addEventListener("pointerup", up);
    });
  }

  // ------------------------------------------------------------ the forces
  let alpha = 0, settle = true;
  function kick(a) { alpha = Math.max(alpha, a); if (!sim) sim = requestAnimationFrame(tick); }
  function tick() {
    sim = null;
    if (!page.isConnected) return;
    const ns = [...nodes.values()];
    const k = 70;
    for (let i = 0; i < ns.length; i++) {
      const a = ns[i];
      for (let j = i + 1; j < ns.length; j++) {
        const b = ns[j];
        let dx = b.x - a.x, dy = b.y - a.y; let d2 = dx * dx + dy * dy;
        if (d2 < 0.01) { dx = Math.random() - 0.5; dy = Math.random() - 0.5; d2 = 0.5; }
        if (d2 > 250000) continue;
        const f = (k * k) / d2 * alpha * 0.9;
        const d = Math.sqrt(d2);
        const fx = dx / d * f, fy = dy / d * f;
        a.vx -= fx; a.vy -= fy; b.vx += fx; b.vy += fy;
        const min = (a.r || 6) + (b.r || 6) + 4;
        if (d < min) { const push = (min - d) / d * 0.5; a.vx -= dx * push; a.vy -= dy * push; b.vx += dx * push; b.vy += dy * push; }
      }
    }
    for (const e of edges.values()) {
      const a = nodes.get(e.s), b = nodes.get(e.t);
      if (!a || !b) continue;
      const dx = b.x - a.x, dy = b.y - a.y, d = Math.hypot(dx, dy) || 1;
      const f = (d - k) / d * 0.06 * alpha * 4;
      a.vx += dx * f; a.vy += dy * f; b.vx -= dx * f; b.vy -= dy * f;
    }
    for (const n of ns) {
      n.vx -= n.x * 0.004 * alpha; n.vy -= n.y * 0.004 * alpha;
      if (n.fixed) { n.vx = n.vy = 0; continue; }
      n.vx *= 0.6; n.vy *= 0.6;
      n.x += Math.max(-30, Math.min(30, n.vx)); n.y += Math.max(-30, Math.min(30, n.vy));
    }
    place();
    alpha *= 0.985;
    if (alpha > 0.01) sim = requestAnimationFrame(tick);
    else if (settle) { settle = false; vp?.fit(40); }
  }

  // ------------------------------------------------------------ actions
  function select(id) {
    sel = id; pathEnds = id ? [id] : [];
    rebuild(); drawSide();
  }
  async function pagerank(id) {
    const sql = `SELECT ${nodeCols().map(quoteName).join(", ")}, score() FROM ${quoteName(g.nodes)} ORDER BY ppr(${G()}, ${lit(id)}${st.dir !== "both" ? `, dir => '${st.dir}'` : ""}) DESC LIMIT 15`;
    lastSql = sql;
    try {
      const j = await api("/v1/query", { body: { sql } });
      const n = nodes.get(id);
      ranks = new Map();
      const before = new Set(nodes.keys());
      j.rows.forEach((r, i) => { const row = Object.fromEntries(j.columns.slice(0, -1).map((c, k) => [c, r[k]])); const m = addNode(row, n); m.ppr = r[r.length - 1]; ranks.set(m.id, i); });
      await linksAmong([...nodes.keys()].filter((k) => !before.has(k)));
      rebuild(); kick(0.4); drawSide();
      toast(`Ranked ${fmt.plural(j.rows.length, "row")} by personalized PageRank from ${id}`, "ok");
    } catch (e) { toast(e.message, "bad"); }
  }
  async function findPath(a, b) {
    const sql = `SELECT ${quoteName(key())}, hops() FROM ${quoteName(g.nodes)} WHERE PATH(${G()}, ${lit(a)}, ${lit(b)}, max_hops => 6, dir => '${g.kind === "sim" ? "out" : st.dir}') ORDER BY hops() LIMIT 10`;
    lastSql = sql;
    try {
      const j = await api("/v1/sql", { body: { sql } });
      if (!j.rows.length) { toast(`No path of 6 links or fewer from ${a} to ${b}`, "warn"); return; }
      path = j.rows.map((r) => String(r[0]));
      const missing = path.filter((k) => !nodes.has(k));
      for (const row of await rowsOf(missing)) addNode(row, nodes.get(a));
      for (let i = 0; i + 1 < path.length; i++) if (!edges.has(`${path[i]}>${path[i + 1]}`) && !edges.has(`${path[i + 1]}>${path[i]}`)) addEdge(path[i], path[i + 1], 1, g.kind !== "sim");
      await linksAmong(missing);
      rebuild(); kick(0.4); drawSide();
      toast(`A path of ${fmt.plural(path.length - 1, "link")} from ${a} to ${b}`, "ok");
    } catch (e) { toast(e.message, "bad"); }
  }

  function drawSide() {
    const n = sel ? nodes.get(sel) : null;
    const deg = degrees();
    if (!n) {
      clear(side, h("div", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Explore")),
        h("div", { class: "bd stack small dim" },
          h("div", {}, h("b", {}, "Click"), " a node to open it, ", h("b", {}, "drag"), " to pin it (double-click frees it), scroll to zoom, drag the background to pan."),
          h("div", {}, h("b", {}, "Shift-click"), " a second node to find the shortest path between them (PATH)."),
          h("div", {}, "Opening a node adds its neighbours; PageRank ranks the whole graph from it (ppr)."),
          lastSql ? [h("div", { class: "small muted" }, "The last walk"), sqlBlock(lastSql)] : null)),
        ranks.size ? rankCard() : null);
      return;
    }
    const row = n.row || {};
    clear(side,
      h("div", { class: "card" },
        h("div", { class: "hd" }, h("span", { class: "gx-ldot", style: { background: colorOf(n), width: "12px", height: "12px" } }), h("h3", { class: "ellipsis" }, String(row[st.label] ?? n.id))),
        h("div", { class: "bd stack" },
          h("div", { class: "small muted mono" }, n.id, ` · ${fmt.plural(deg.get(n.id) || 0, "link")} shown`),
          h("table", { class: "t compact" }, h("tbody", {}, Object.entries(row).filter(([k]) => k !== key()).map(([k, v]) => h("tr", {}, h("td", { class: "muted" }, k), h("td", {}, v == null ? "–" : String(v)))))),
          h("div", { class: "row wrap", style: { gap: "6px" } },
            h("button", { class: "btn sm primary", onclick: async () => { const k = await expand(n.id, 30); drawSide(); toast(`${fmt.plural(k, "link")} of ${n.id}`, "ok"); } }, icon("plus"), n.expanded ? "More neighbours" : "Open neighbours"),
            g.kind !== "sim" ? h("button", { class: "btn sm", onclick: () => pagerank(n.id) }, icon("sparkle"), "PageRank from here") : null,
            h("button", { class: "btn sm ghost", onclick: () => { n.fixed = !n.fixed; rebuild(); kick(0.2); drawSide(); } }, icon(n.fixed ? "unlock" : "lock"), n.fixed ? "Unpin" : "Pin"),
            h("button", { class: "btn sm ghost", onclick: () => { nodes.delete(n.id); for (const [k, e] of edges) if (e.s === n.id || e.t === n.id) edges.delete(k); sel = null; rebuild(); drawSide(); } }, icon("trash"), "Remove")),
          h("div", { class: "rl-qwrap", style: { padding: "0 10px", flex: "none" } }, icon("route"), h("input", { class: "input rl-q", style: { fontSize: "13px", padding: "6px 0" }, placeholder: "Shortest path to a key (or shift-click a node)", onkeydown: (e) => { if (e.key === "Enter" && e.target.value.trim()) findPath(n.id, e.target.value.trim()); } })),
          path.length ? h("div", {}, h("div", { class: "small muted" }, `The shortest path, ${fmt.plural(path.length - 1, "link")}`), h("ol", { class: "gx-steps" }, path.map((k) => h("li", { tabindex: 0, onclick: () => select(k), onkeydown: (e) => { if (e.key === "Enter") select(k); } }, h("span", { class: "ellipsis" }, String(nodes.get(k)?.row?.[st.label] ?? k)), h("span", { class: "mono small muted" }, k))))) : null,
          lastSql ? [h("div", { class: "small muted" }, "The last walk"), sqlBlock(lastSql)] : null)),
      ranks.size ? rankCard() : null);
  }
  function rankCard() {
    const list = [...ranks.entries()].sort((a, b) => a[1] - b[1]);
    const max = Math.max(...list.map(([k]) => nodes.get(k)?.ppr || 0), 1e-12);
    return h("div", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "PageRank from the node"), h("div", { class: "tools" }, h("button", { class: "btn xs ghost", onclick: () => { ranks = new Map(); rebuild(); drawSide(); } }, "Clear"))),
      h("div", { class: "bd flush" }, list.map(([k, i]) => { const m = nodes.get(k); return h("div", { class: "gx-rrow", onclick: () => select(k) }, h("span", { class: "rl-n" }, i + 1), h("span", { class: "ellipsis grow" }, String(m?.row?.[st.label] ?? k)), h("span", { class: "sd-cbar", style: { width: "70px" } }, h("i", { style: { width: ((m?.ppr || 0) / max * 100) + "%" } })), h("span", { class: "num small dim", style: { whiteSpace: "nowrap" } }, Number((m?.ppr ?? 0).toPrecision(2)))); })));
  }

  return () => { if (sim) cancelAnimationFrame(sim); sim = null; };
}
