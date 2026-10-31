// Topology: the cluster as a diagram. A coordinator over its shards, each
// shard's copies (the primary and its replicas, how far behind each is),
// the calls flowing to each shard, and the ring of 4,096 slots. Drag a
// shard to arrange the picture (kept in this browser), scroll to zoom,
// drag the background to pan, click a copy for its details. A server that
// is not a coordinator shows how its writes reach storage and its copies.

import { h, s, icon, clear, fmt, card, store, drawer, copy, pill, showTip, hideTip, clamp, bus, empty } from "../core.js";
import { SERIES } from "../charts.js";
import { state, isCoordinator, refreshCluster, last } from "../data.js";

export default { id: "topology", title: "Topology", icon: "topology", group: "Monitor", keywords: "cluster shards replicas diagram map slots ring replication", render };

const NW = 240, NH = 74, PAD = 14, LBL = 30, GAP = 12, COLGAP = 56;
const short = (url) => (url || "").replace(/^https?:\/\//, "");

// ---------------------------------------------------------------- layout
function layout(cluster, saved) {
  const shards = cluster.shards || [];
  const gw = NW + PAD * 2;
  const total = shards.length * gw + Math.max(0, shards.length - 1) * COLGAP;
  const coord = saved.coord || { x: total / 2 - NW / 2, y: 10 };
  const groups = shards.map((sh, i) => {
    const order = [...sh.replicas.keys()].sort((a, b) => (sh.replicas[b].takes_writes ? 1 : 0) - (sh.replicas[a].takes_writes ? 1 : 0));
    const gh = LBL + order.length * NH + (order.length - 1) * GAP + PAD;
    const pos = saved["g" + sh.url] || { x: i * (gw + COLGAP), y: 170 };
    return { sh, i, x: pos.x, y: pos.y, w: gw, h: gh, order };
  });
  return { coord, groups };
}

/** The replication state the primary of a shard reports of each replica, by URL. */
function followers(sh) {
  const p = sh.replicas.find((r) => r.takes_writes) || sh.replicas[0];
  const reps = p?.detail?.health?.replication?.replicas || [];
  const by = {};
  sh.replicas.filter((r) => r !== p).forEach((r, i) => { by[r.url] = reps.find((x) => x.url === r.url) || reps[i] || null; });
  return by;
}

// ---------------------------------------------------------------- drawing
function nodeBox(x, y, { title, sub, vals, status, badge, sel, onclick, icon: ic }) {
  const g = s("g", { class: `node ${status || ""}${sel ? " sel" : ""}`, transform: `translate(${x},${y})`, tabindex: 0, role: "button", "aria-label": title });
  g.append(
    s("rect", { class: "box", width: NW, height: NH, rx: 10 }),
    s("rect", { class: "stripe", width: 4, height: NH - 20, x: 0, y: 10, rx: 2 }),
    s("text", { class: "t", x: 16, y: 22 }, title),
    s("text", { class: "s", x: 16, y: 40 }, sub || ""),
    s("text", { class: "v", x: 16, y: 60 }, vals || ""));
  if (badge) {
    const w = badge.length * 6.4 + 14;
    g.append(s("rect", { x: NW - w - 10, y: 10, width: w, height: 18, rx: 9, style: { fill: badge === "primary" || badge === "coordinator" ? "var(--accent-soft)" : "var(--surface-3)" } }),
      s("text", { x: NW - w / 2 - 10, y: 23, "text-anchor": "middle", style: { fontSize: "10.5px", fontWeight: 600, fill: badge === "primary" || badge === "coordinator" ? "var(--accent)" : "var(--text-2)" } }, badge));
  }
  if (ic) { const i = s("g", { transform: `translate(${NW - 30},${NH - 28}) scale(.75)`, style: { color: "var(--muted)" } }); i.innerHTML = icon(ic).innerHTML; i.setAttribute("fill", "none"); i.setAttribute("stroke", "currentColor"); i.setAttribute("stroke-width", "1.8"); g.append(i); }
  if (onclick) { g.addEventListener("click", (e) => { if (!g.moved) onclick(e); }); g.addEventListener("keydown", (e) => { if (e.key === "Enter") onclick(e); }); }
  return g;
}

function edge(x1, y1, x2, y2, cls, label, speed) {
  const my = (y1 + y2) / 2;
  const d = `M${x1},${y1} C${x1},${my} ${x2},${my} ${x2},${y2}`;
  const g = s("g");
  const p = s("path", { class: "edge " + cls, d });
  if (speed) p.style.animationDuration = speed + "s";
  g.append(s("path", { class: "edge", d, style: { strokeOpacity: 0.35 } }), p);
  if (label) {
    const tx = (x1 + x2) / 2, ty = my;
    const w = label.length * 6 + 12;
    g.append(s("rect", { class: "edge-lbl-bg", x: tx - w / 2, y: ty - 9, width: w, height: 18, rx: 9 }), s("text", { class: "edge-lbl", x: tx, y: ty + 4, "text-anchor": "middle" }, label));
  }
  return g;
}

/** Pan and zoom of a <g> inside an <svg>. */
export function viewport(svgEl, root, { mini } = {}) {
  let k = 1, x = 0, y = 0;
  const apply = () => root.setAttribute("transform", `translate(${x.toFixed(1)},${y.toFixed(1)}) scale(${k.toFixed(3)})`);
  const vp = {
    get k() { return k; },
    fit(pad = 24) {
      const b = root.getBBox(); const r = svgEl.getBoundingClientRect();
      if (!b.width || !r.width) return;
      k = clamp(Math.min((r.width - pad * 2) / b.width, (r.height - pad * 2) / b.height), 0.25, mini ? 1 : 1.4);
      x = (r.width - b.width * k) / 2 - b.x * k; y = (r.height - b.height * k) / 2 - b.y * k; apply();
    },
    zoom(f, px, py) { const r = svgEl.getBoundingClientRect(); px ??= r.width / 2; py ??= r.height / 2; const nk = clamp(k * f, 0.25, 3); x = px - (px - x) * nk / k; y = py - (py - y) * nk / k; k = nk; apply(); },
    get state() { return { k, x, y }; }, set state(v) { ({ k, x, y } = v); apply(); },
  };
  if (!mini) {
    svgEl.addEventListener("wheel", (e) => { e.preventDefault(); const r = svgEl.getBoundingClientRect(); vp.zoom(Math.exp(-e.deltaY * 0.0015), e.clientX - r.left, e.clientY - r.top); }, { passive: false });
    svgEl.addEventListener("pointerdown", (e) => {
      if (e.target !== svgEl && !e.target.classList.contains("bg")) return;
      const sx = e.clientX - x, sy = e.clientY - y;
      svgEl.classList.add("panning"); svgEl.setPointerCapture(e.pointerId);
      const move = (ev) => { x = ev.clientX - sx; y = ev.clientY - sy; apply(); };
      const up = () => { svgEl.classList.remove("panning"); svgEl.removeEventListener("pointermove", move); svgEl.removeEventListener("pointerup", up); };
      svgEl.addEventListener("pointermove", move); svgEl.addEventListener("pointerup", up);
    });
  }
  apply();
  return vp;
}

/** Makes a <g> draggable in content space; onMove(dx, dy), onEnd(). */
export function draggable(g, vp, onMove, onEnd) {
  g.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.stopPropagation();
    let lx = e.clientX, ly = e.clientY, moved = 0;
    g.setPointerCapture(e.pointerId);
    const move = (ev) => { const dx = (ev.clientX - lx) / vp.k, dy = (ev.clientY - ly) / vp.k; lx = ev.clientX; ly = ev.clientY; moved += Math.abs(dx) + Math.abs(dy); if (moved > 3) { g.classList.add("dragging"); onMove(dx, dy); } };
    const up = () => { g.removeEventListener("pointermove", move); g.removeEventListener("pointerup", up); g.classList.remove("dragging"); g.moved = moved > 3; setTimeout(() => (g.moved = false), 0); if (moved > 3) onEnd(); };
    g.addEventListener("pointermove", move); g.addEventListener("pointerup", up);
  });
}

/** Draws a coordinator's cluster into `host`; mini: a fitted picture without editing. */
function drawCluster(host, cluster, { mini = false, onPick, selected } = {}) {
  const saved = mini ? {} : store.get("topo.layout", {});
  const L = layout(cluster, saved);
  const svgEl = s("svg", { role: "img", "aria-label": "cluster topology", style: { width: "100%", height: "100%", display: "block" } });
  const root = s("g");
  svgEl.append(s("rect", { class: "bg", x: -5000, y: -5000, width: 10000, height: 10000, fill: "transparent" }), root);
  clear(host, svgEl);
  const vp = viewport(svgEl, root, { mini });
  const co = cluster.coordinator?.load || {};
  const edges = s("g"), nodes = s("g");
  root.append(edges, nodes);

  function paint() {
    edges.replaceChildren(); nodes.replaceChildren();
    const cx = L.coord.x + NW / 2, cy = L.coord.y + NH;
    for (const g of L.groups) {
      const sh = g.sh;
      const rate = sh.call_rate;
      const rtts = sh.replicas.map((r) => r.rtt_ms).filter((x) => x != null);
      const label = mini ? null : `${rate == null ? "…" : fmt.si(rate) + " calls/s"} · ${rtts.length ? fmt.ms(Math.min(...rtts)) : "–"}`;
      const cls = sh.status === "down" ? "down" : "flow";
      const speed = rate ? clamp(2.2 - Math.log10(1 + rate) * 0.6, 0.35, 2.2) : 2.4;
      edges.append(edge(cx, cy, g.x + g.w / 2, g.y, cls, label, speed));
      // The shard's frame.
      const slots = (sh.slots || []).reduce((a, [x, y]) => a + (y - x + 1), 0);
      const frame = s("g", { transform: `translate(${g.x},${g.y})`, class: "node", style: { cursor: mini ? "pointer" : "move" } });
      frame.append(
        s("rect", { width: g.w, height: g.h, rx: 14, style: { fill: "color-mix(in srgb, var(--surface) 55%, transparent)", stroke: sh.status === "ok" ? "var(--line-2)" : sh.status === "degraded" ? "var(--warn)" : "var(--crit)", strokeDasharray: "5 4", strokeWidth: 1.4 } }),
        s("circle", { cx: 16, cy: 16, r: 5, style: { fill: SERIES[g.i % 8] } }),
        s("text", { x: 28, y: 20, style: { fontWeight: 650, fontSize: "12.5px", fill: "var(--text)" } }, `shard ${sh.shard}`),
        s("text", { x: g.w - 12, y: 20, "text-anchor": "end", style: { fontSize: "11px", fill: "var(--muted)" } }, slots ? `${fmt.pct(slots / (cluster.slot_count || 4096), 0)} of slots` : ""));
      nodes.append(frame);
      if (!mini) draggable(frame, vp, (dx, dy) => { g.x += dx; g.y += dy; paint(); }, () => { const sv = store.get("topo.layout", {}); sv["g" + sh.url] = { x: g.x, y: g.y }; store.set("topo.layout", sv); });
      else frame.addEventListener("click", () => (location.hash = "#/topology"));
      // Its copies.
      const fol = followers(sh);
      const primary = sh.replicas.find((r) => r.takes_writes);
      g.order.forEach((ri, j) => {
        const r = sh.replicas[ri];
        const x = g.x + PAD, y = g.y + LBL + j * (NH + GAP);
        const load = r.detail?.load || {};
        const role = r.role === "primary" || r.takes_writes ? (sh.replicas.length > 1 ? "primary" : "shard") : r.role || "replica";
        const f = fol[r.url];
        const st = r.status === "ok" ? (f && !f.in_sync ? "degraded" : "ok") : r.status;
        nodes.append(nodeBox(x, y, {
          title: short(r.url), status: st, badge: role, sel: selected === r.url,
          sub: r.status === "down" ? (r.error || "down") : `${fmt.si(r.docs)} rows · ${fmt.plural(r.segments, "segment")}${r.avoided ? " · avoided" : ""}`,
          vals: r.status === "down" ? "" : `${fmt.si(load.qps)} q/s · p99 ${fmt.ms(load.p99_ms)} · ${fmt.ms(r.rtt_ms)} rtt`,
          onclick: mini ? () => (location.hash = "#/topology") : () => onPick?.(r, sh),
        }));
        if (primary && r !== primary && j > 0) {
          const pj = g.order.indexOf(sh.replicas.indexOf(primary));
          const py = g.y + LBL + pj * (NH + GAP) + NH / 2, ry = y + NH / 2, x1 = g.x + PAD + NW;
          const lag = f ? (f.in_sync ? "in sync" : `${fmt.si(f.behind)} behind`) : "";
          const p = s("path", { class: "edge repl" + (f && !f.in_sync ? " lag" : "") + (r.status === "down" ? " down" : ""), d: `M${x1},${py} C${x1 + 34},${py} ${x1 + 34},${ry} ${x1},${ry}` });
          edges.append(p);
          if (!mini && lag) edges.append(s("text", { class: "edge-lbl", x: x1 + 30, y: (py + ry) / 2 + 4 }, lag));
        }
      });
    }
    // The coordinator.
    const coordNode = nodeBox(L.coord.x, L.coord.y, {
      title: location.host, badge: "coordinator", status: cluster.status, sel: selected === "coordinator",
      sub: `${fmt.plural(cluster.shards.length, "shard")} · epoch ${cluster.slot_epoch ?? "–"} · protocol ${cluster.protocol ?? "–"}`,
      vals: `${fmt.si(co.qps)} q/s · p99 ${fmt.ms(co.p99_ms)} · ${fmt.n(cluster.hedges)} hedges`,
      onclick: mini ? () => (location.hash = "#/topology") : () => onPick?.(null),
    });
    nodes.append(coordNode);
    if (!mini) draggable(coordNode, vp, (dx, dy) => { L.coord.x += dx; L.coord.y += dy; paint(); }, () => { const sv = store.get("topo.layout", {}); sv.coord = L.coord; store.set("topo.layout", sv); });
  }
  paint();
  requestAnimationFrame(() => { const saved = !mini && store.get("topo.view", null); if (saved && host.dataset.fitted) vp.state = saved; else vp.fit(); host.dataset.fitted = "1"; });
  if (!mini) svgEl.addEventListener("pointerup", () => store.set("topo.view", vp.state));
  return vp;
}

export function topoMini(host, cluster) { drawCluster(host, cluster, { mini: true }); }

// ---------------------------------------------------------------- slots
function slotRing(cluster, size = 220) {
  const n = cluster.slot_count || 4096;
  const r = size / 2 - 6, ri = r - 26, c = size / 2;
  const svgEl = s("svg", { viewBox: `0 0 ${size} ${size}`, width: size, height: size, role: "img", "aria-label": "slots by shard" });
  const p = (a, rr) => `${(c + rr * Math.cos(a)).toFixed(2)},${(c + rr * Math.sin(a)).toFixed(2)}`;
  const arcs = [];
  (cluster.shards || []).forEach((sh, i) => (sh.slots || []).forEach(([a, b]) => arcs.push({ a, b, i, sh })));
  for (const { a, b, i, sh } of arcs) {
    const a0 = a / n * 2 * Math.PI - Math.PI / 2 + 0.004, a1 = (b + 1) / n * 2 * Math.PI - Math.PI / 2 - 0.004;
    const large = a1 - a0 > Math.PI ? 1 : 0;
    const path = s("path", { d: `M${p(a0, r)}A${r},${r} 0 ${large} 1 ${p(a1, r)}L${p(a1, ri)}A${ri},${ri} 0 ${large} 0 ${p(a0, ri)}Z`, style: { fill: SERIES[i % 8] }, tabindex: 0 });
    path.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `shard ${sh.shard}`), h("div", { class: "r" }, h("span", { class: "k box", style: { background: SERIES[i % 8] } }), h("b", {}, `slots ${a}–${b}`), h("span", { class: "n" }, fmt.pct((b - a + 1) / n))), h("div", { class: "r" }, h("span", { class: "n", style: { marginLeft: 0 } }, short(sh.url)))]));
    path.addEventListener("pointerleave", hideTip);
    svgEl.append(path);
  }
  svgEl.append(s("text", { x: c, y: c - 2, "text-anchor": "middle", style: { fill: "var(--text)", fontSize: "22px", fontWeight: 650 } }, fmt.n(n)),
    s("text", { x: c, y: c + 16, "text-anchor": "middle", style: { fill: "var(--muted)", fontSize: "11px" } }, `slots · epoch ${cluster.slot_epoch ?? "–"}`));
  return svgEl;
}

// ---------------------------------------------------------------- one server
/**
 * How one server's writes reach storage and its queries read it: a
 * pipeline with dots moving at the rates of the latest point.
 */
export function storageFlow(host, p, health, system) {
  const W = Math.max(560, host.clientWidth || 760), H = Math.max(260, host.clientHeight || 300);
  const reps = health?.replication?.role === "primary" ? health.replication.replicas || [] : [];
  let svgEl = host.querySelector("svg.flow");
  const fresh = !svgEl || Math.abs(+svgEl.dataset.w - W) > 4 || +svgEl.dataset.reps !== reps.length;
  const cols = 5, cw = (W - 40) / cols, bw = Math.min(150, cw - 26), bh = 70;
  const X = (i) => 20 + i * cw + (cw - bw) / 2;
  const yW = 34, yR = H - bh - 30;
  const stages = [
    { id: "in", x: X(0), y: (yW + yR) / 2, t: "Clients", s: "HTTP and gRPC", v: () => fmt.si(p.http_s) + "/s" },
    { id: "queue", x: X(1), y: yW, t: "Write queue", s: `${fmt.si(p.writes_s)} writes/s`, v: () => fmt.n(p.write_queue ?? 0) },
    { id: "wal", x: X(2), y: yW, t: "Write-ahead log", s: "group commit", v: () => fmt.bytes(p.wal_bytes) },
    { id: "mem", x: X(3), y: yW, t: "Memtable", s: "rows not flushed", v: () => fmt.si(p.memtable_docs ?? 0) },
    { id: "seg", x: X(4), y: (yW + yR) / 2, t: "Segments", s: `${fmt.n(p.segments)} · ${fmt.si(p.deleted_docs ?? 0)} deleted`, v: () => fmt.bytes(p.file_bytes) },
    { id: "adm", x: X(1), y: yR, t: "Admission", s: system ? `${p.slots_free ?? "–"} of ${system.limits.max_concurrent} slots free` : "slots", v: () => fmt.n(p.running ?? 0) + " running" },
    { id: "exec", x: X(2) + (X(3) - X(2)) / 2, y: yR, t: "Execute", s: `${fmt.si(p.qps)} q/s · p99 ${fmt.ms(p.p99_ms)}`, v: () => fmt.n(p.pool_queued ?? 0) + " queued" },
  ];
  const at = (id) => stages.find((x) => x.id === id);
  const links = [
    ["in", "queue", "w"], ["queue", "wal", "w"], ["wal", "mem", "w"], ["mem", "seg", "w"],
    ["in", "adm", "r"], ["adm", "exec", "r"], ["exec", "seg", "r"],
  ];
  const mid = (a, side) => side === "r" ? [a.x + bw, a.y + bh / 2] : side === "l" ? [a.x, a.y + bh / 2] : side === "t" ? [a.x + bw / 2, a.y] : [a.x + bw / 2, a.y + bh];
  const path = (a, b) => {
    const [x1, y1] = mid(a, "r"), [x2, y2] = mid(b, "l");
    const mx = (x1 + x2) / 2;
    return { d: `M${x1},${y1} C${mx},${y1} ${mx},${y2} ${x2},${y2}`, x1, y1, x2, y2 };
  };
  if (fresh) {
    svgEl = s("svg", { class: "flow", viewBox: `0 0 ${W} ${H}`, style: { width: "100%", height: H + "px" }, dataset: { w: W, reps: reps.length } });
    host.replaceChildren(svgEl);
    const lanes = s("g");
    lanes.append(s("text", { class: "flow-sub", x: 20, y: 18 }, "WRITES"), s("text", { class: "flow-sub", x: 20, y: H - 8 }, "QUERIES"));
    svgEl.append(lanes);
    for (const [a, b, k] of links) {
      const pth = path(at(a), at(b));
      svgEl.append(s("path", { class: "edge", d: pth.d, dataset: { k, a, b }, id: `fl-${a}-${b}` }));
    }
    reps.forEach((r, i) => {
      const wal = at("wal");
      svgEl.append(s("path", { class: "edge repl", d: `M${wal.x + bw / 2},${wal.y + bh} L${wal.x + bw / 2},${H / 2 - 4 + i * 22}` }));
    });
    const g = s("g", { class: "stages" });
    svgEl.append(g, s("g", { class: "parts" }));
  }
  const g = svgEl.querySelector(".stages");
  g.replaceChildren();
  for (const st of stages) {
    const warn = (st.id === "queue" && p.write_queue > 0) || (st.id === "adm" && p.slots_free === 0) || (st.id === "seg" && p.merge_failing) || (st.id === "mem" && p.flush_failing);
    const gg = s("g", { transform: `translate(${st.x},${st.y})` });
    gg.append(s("rect", { class: "flow-stage", width: bw, height: bh, rx: 10, style: { stroke: warn ? "var(--warn)" : null, strokeWidth: warn ? 2 : 1 } }),
      s("text", { class: "flow-lbl", x: 12, y: 20 }, st.t), s("text", { class: "flow-val", x: 12, y: 44 }, st.v()), s("text", { class: "flow-sub", x: 12, y: 61 }, st.s));
    if (st.id === "seg" && p.merges_running) gg.append(s("text", { class: "flow-sub", x: bw - 10, y: 20, "text-anchor": "end", style: { fill: "var(--s3)" } }, `${p.merges_running} merging`));
    g.append(gg);
  }
  if (reps.length) {
    const wal = at("wal");
    reps.forEach((r, i) => {
      const y = H / 2 - 4 + i * 22;
      const gg = s("g", { transform: `translate(${wal.x + bw / 2 - 70},${y})` });
      gg.append(s("rect", { width: 140, height: 18, rx: 9, style: { fill: r.in_sync ? "var(--good-soft)" : "var(--warn-soft)", stroke: "var(--line)" } }),
        s("text", { x: 70, y: 13, "text-anchor": "middle", style: { fontSize: "10.5px", fill: "var(--text)" } }, `${short(r.url)} · ${r.in_sync ? "in sync" : fmt.si(r.behind) + " behind"}`));
      g.append(gg);
    });
  }
  // Dots moving along the links at the point's rates.
  const parts = svgEl.querySelector(".parts");
  const rate = { w: p.rows_s || p.writes_s || 0, r: p.qps || 0 };
  if (!svgEl.anim) {
    svgEl.anim = { dots: [], last: performance.now() };
    const tick = (now) => {
      if (!svgEl.isConnected) return;
      const a = svgEl.anim, dt = Math.min(0.1, (now - a.last) / 1000); a.last = now;
      for (const k of ["w", "r"]) {
        const n = a.rate?.[k] || 0;
        a.acc = a.acc || { w: 0, r: 0 };
        a.acc[k] += Math.min(12, Math.log2(1 + n) * 1.3) * dt;
        while (a.acc[k] >= 1 && a.dots.length < 80) {
          a.acc[k] -= 1;
          const route = k === "w" ? ["in-queue", "queue-wal", "wal-mem", "mem-seg"] : ["in-adm", "adm-exec", "exec-seg"];
          const dot = { route, step: 0, t: 0, el: s("circle", { r: 3.2, class: "particle" + (k === "w" ? " w" : "") }) };
          a.dots.push(dot); parts.append(dot.el);
        }
      }
      a.dots = a.dots.filter((d) => {
        d.t += dt * 1.1;
        if (d.t >= 1) { d.t = 0; d.step++; }
        if (d.step >= d.route.length) { d.el.remove(); return false; }
        const pe = svgEl.querySelector("#fl-" + d.route[d.step]);
        if (!pe) { d.el.remove(); return false; }
        const len = pe.getTotalLength(); const pt = pe.getPointAtLength(d.t * len);
        d.el.setAttribute("cx", pt.x); d.el.setAttribute("cy", pt.y);
        return true;
      });
      requestAnimationFrame(tick);
    };
    if (!matchMedia("(prefers-reduced-motion: reduce)").matches) requestAnimationFrame(tick);
  }
  svgEl.anim.rate = rate;
}

// ---------------------------------------------------------------- the view
function render(page) {
  const head = h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Topology"), h("p", { class: "lead" }, "")));
  const body = h("div");
  page.append(head, body);
  let vp = null, selected = null, built = null;

  const diagram = h("div", { class: "diagram", style: { height: "calc(100vh - 230px)", minHeight: "460px" } });
  const controls = h("div", { class: "controls" },
    h("button", { class: "btn icon sm", title: "Zoom in", onclick: () => vp?.zoom(1.25) }, icon("zin")),
    h("button", { class: "btn icon sm", title: "Zoom out", onclick: () => vp?.zoom(0.8) }, icon("zout")),
    h("button", { class: "btn icon sm", title: "Fit", onclick: () => vp?.fit() }, icon("fit")),
    h("button", { class: "btn icon sm", title: "Reset the layout", onclick: () => { store.del("topo.layout"); store.del("topo.view"); delete diagramHost.dataset.fitted; draw(); } }, icon("refresh")));
  const diagramHost = h("div", { style: { position: "absolute", inset: 0 } });
  diagram.append(diagramHost, controls,
    h("div", { class: "legend-box" },
      h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "dot ok" }), "ok"),
      h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "dot warn" }), "degraded or behind"),
      h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "dot bad" }), "down"),
      h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { style: { width: "18px", borderTop: "2px dashed var(--s1)" } }), "queries"),
      h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { style: { width: "18px", borderTop: "2px dashed var(--s3)" } }), "replication"),
      h("span", { class: "muted" }, "drag shards · scroll to zoom · drag the background to pan")));

  function pick(r, sh) {
    selected = r ? r.url : "coordinator";
    draw();
    const c = state.cluster;
    if (!r) {
      const co = c.coordinator?.load || {};
      drawer({
        title: "Coordinator", sub: location.host,
        body: [
          h("dl", { class: "kv" }, [["status", pill(c.status)], ["shards", c.shards.length], ["slot epoch", c.slot_epoch], ["protocol", c.protocol], ["retries", fmt.n(c.retries)], ["hedges", `${fmt.n(c.hedges)} (after ${fmt.ms(c.hedge_ms)})`], ["queries/s", fmt.si(co.qps)], ["p99", fmt.ms(co.p99_ms)], ["CPU", fmt.si(co.cpu) + " cores"], ["memory", fmt.bytes(co.rss_bytes)]].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
          card({ title: "Calls to each shard", body: h("div", { class: "stack" }, c.shards.map((s2, i) => h("div", { class: "row nw small" }, h("span", { class: "dot", style: { background: SERIES[i % 8] } }), h("b", {}, `shard ${s2.shard}`), h("span", { class: "grow dim" }, `${fmt.n(s2.calls.ok)} ok · ${fmt.n(s2.calls.error)} errors · ${fmt.n(s2.calls.timeout)} timeouts`)))) }),
        ],
        onClose: () => { selected = null; draw(); },
      });
      return;
    }
    const d = r.detail || {}, load = d.load || {}, rep = d.health?.replication;
    const fol = followers(sh)[r.url];
    drawer({
      title: short(r.url), sub: `shard ${sh.shard} · ${r.takes_writes ? "takes writes" : "read copy"}`,
      body: [
        h("div", { class: "row" }, pill(r.status), r.avoided ? pill("warn", "avoided after errors") : null, fol ? pill(fol.in_sync ? "ok" : "warn", fol.in_sync ? "in sync" : `${fmt.n(fol.behind)} behind`) : null),
        r.error ? h("div", { class: "error" }, r.error) : null,
        h("dl", { class: "kv" }, [
          ["role", r.role], ["rows", fmt.n(r.docs)], ["segments", fmt.n(r.segments)], ["transaction", fmt.n(r.txn)], ["round trip", fmt.ms(r.rtt_ms)], ["protocol", r.protocol],
          ["version", d.system?.version], ["up", fmt.dur(d.system?.uptime_s)], ["cores", d.system?.cores], ["running", d.running],
        ].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v ?? "–")])),
        card({ title: "Load", sub: "its latest point", body: h("dl", { class: "kv" }, [
          ["queries/s", fmt.si(load.qps)], ["p50 · p99", `${fmt.ms(load.p50_ms)} · ${fmt.ms(load.p99_ms)}`], ["writes/s", fmt.si(load.writes_s)], ["CPU", fmt.si(load.cpu) + " cores"], ["memory", fmt.bytes(load.rss_bytes)],
          ["disk free", fmt.bytes(load.disk_free_bytes)], ["file", fmt.bytes(load.file_bytes)], ["write-ahead log", fmt.bytes(load.wal_bytes)], ["memtable", fmt.si(load.memtable_docs) + " rows"], ["merges", load.merges_running],
        ].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v ?? "–")])) }),
        rep ? card({ title: "Replication", sub: rep.role, body: rep.role === "primary"
          ? h("div", { class: "stack" }, h("dl", { class: "kv" }, [["term", rep.term], ["sequence", fmt.n(rep.seq_no)], ["global checkpoint", fmt.n(rep.global_checkpoint)], ["write acks", rep.write_acks]].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v ?? "–")])),
            (rep.replicas || []).map((x) => h("div", { class: "row nw small" }, h("span", { class: "dot " + (x.in_sync ? "ok" : "warn") }), h("span", { class: "mono grow ellipsis" }, short(x.url)), h("span", { class: "dim" }, `${x.in_sync ? "in sync" : fmt.n(x.behind) + " behind"} · ${x.copies} copies · ${x.divergences} divergences`))))
          : h("dl", { class: "kv" }, [["term", rep.term], ["sequence", fmt.n(rep.seq_no)], ["follows", rep.follows?.primary], ["serves reads", rep.serves_reads ? "yes" : "no"], ["read waits", fmt.n(rep.read_waits)], ["reads refused", fmt.n(rep.reads_refused)], ["copies restored", fmt.n(rep.restores)], ["last shipped", rep.called_ms_ago != null ? fmt.ms(rep.called_ms_ago) + " ago" : "–"]].flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v ?? "–")])) }) : null,
        card({ title: "Operate it", sub: "from a shell that reaches it", body: h("div", { class: "stack", style: { gap: "8px" } },
          !r.takes_writes ? cmd(`strata-server promote --replica ${r.url} --token "$STRATA_TOKEN"`, "Make this replica the primary of a new term") : null,
          cmd(`curl -s -H "authorization: Bearer $STRATA_TOKEN" -X POST http://<its HTTP address>/v1/admin/checkpoint`, "Write a checkpoint on it"),
          cmd(`curl -s -H "authorization: Bearer $STRATA_TOKEN" http://<its HTTP address>/v1/stats`, "Its storage statistics")) }),
      ],
      onClose: () => { selected = null; draw(); },
    });
  }
  const cmd = (text, what) => h("div", {}, h("div", { class: "small muted", style: { marginBottom: "3px" } }, what), h("div", { class: "row nw" }, h("pre", { class: "code grow", style: { padding: "6px 9px" } }, text), h("button", { class: "btn icon sm", title: "Copy", onclick: () => copy(text) }, icon("copy"))));

  function draw() {
    const c = state.cluster;
    if (!c) { clear(body, empty("topology", "Asking the cluster…")); return; }
    if (c.role === "coordinator") {
      head.querySelector("p").textContent = `A coordinator over ${fmt.plural(c.shards.length, "shard")}, ${fmt.plural(c.shards.reduce((a, x) => a + x.replicas.length, 0), "copy", "copies")}. ${c.status === "ok" ? "Every copy answers." : "Some copies do not answer."}`;
      if (built !== "coord") {
        built = "coord";
        const shardTable = h("div");
        const ring = h("div", { class: "row nw", style: { gap: "18px", alignItems: "center" } });
        clear(body, diagram, h("div", { class: "grid g3", style: { marginTop: "14px" } },
          card({ title: "Slots", sub: "which shard owns each of the hash slots", body: ring }),
          h("div", { class: "span2", style: { display: "contents" } }, card({ title: "Shards", body: shardTable, flush: true, cls: "span2" }))));
        body.ring = ring; body.shardTable = shardTable;
      }
      vp = drawCluster(diagramHost, c, { onPick: pick, selected });
      clear(body.ring, slotRing(c), h("div", { class: "ring-legend" }, c.shards.map((sh, i) => {
        const n = (sh.slots || []).reduce((a, [x, y]) => a + (y - x + 1), 0);
        return h("div", { class: "r" }, h("span", { class: "sw", style: { background: SERIES[i % 8] } }), h("b", {}, `shard ${sh.shard}`), h("span", { class: "dim" }, `${fmt.n(n)} slots · ${fmt.pct(n / (c.slot_count || 4096), 0)}`));
      })));
      clear(body.shardTable, h("div", { class: "tbl-wrap" }, h("table", { class: "t" },
        h("thead", {}, h("tr", {}, ["shard", "status", "copies", "rows", "segments", "calls ok", "errors", "timeouts", "slots"].map((x, i) => h("th", { class: i > 2 ? "num" : "" }, x)))),
        h("tbody", {}, c.shards.map((sh, i) => h("tr", {},
          h("td", {}, h("span", { class: "row nw" }, h("span", { class: "dot", style: { background: SERIES[i % 8] } }), `shard ${sh.shard}`)),
          h("td", {}, pill(sh.status)),
          h("td", {}, sh.replicas.map((r) => h("span", { class: "dot " + (r.status === "ok" ? "ok" : r.status === "down" ? "bad" : "warn"), title: `${short(r.url)}: ${r.status}`, style: { marginRight: "4px" } }))),
          h("td", { class: "num" }, fmt.n(Math.max(0, ...sh.replicas.map((r) => r.docs || 0)))),
          h("td", { class: "num" }, fmt.n(Math.max(0, ...sh.replicas.map((r) => r.segments || 0)))),
          h("td", { class: "num" }, fmt.n(sh.calls.ok)), h("td", { class: "num" }, fmt.n(sh.calls.error)), h("td", { class: "num" }, fmt.n(sh.calls.timeout)),
          h("td", { class: "num mono small" }, (sh.slots || []).map(([a, b]) => `${a}–${b}`).join(", "))))))));
    } else {
      built = "one";
      const hl = state.health || {}, rep = hl.replication;
      head.querySelector("p").textContent = rep ? (rep.role === "primary" ? `A primary shipping to ${fmt.plural(rep.replicas.length, "replica")}, term ${rep.term}.` : `A replica following ${rep.follows?.primary || "its primary"}, term ${rep.term}.`) : hl.slots ? `Shard ${hl.slots.shard} of ${hl.slots.of}, slot epoch ${hl.slots.epoch}. Its coordinator shows the whole cluster.` : "One server, not in a cluster. Writes go through the log to the memtable and then to segments; queries read both.";
      if (!body.flow) {
        const flow = h("div", { style: { minHeight: "300px" } });
        const repl = h("div");
        clear(body, card({ title: "How this server stores and serves", sub: "dots move at the latest rates", body: flow }), repl);
        body.flow = flow; body.repl = repl;
      }
      storageFlow(body.flow, last() || {}, hl, state.system);
      if (rep?.role === "primary") {
        clear(body.repl, card({ title: "Replicas", body: table2(["replica", "state", "sequence", "behind", "copies sent", "verified", "divergences"], rep.replicas.map((x) => [h("span", { class: "mono" }, short(x.url)), pill(x.in_sync ? "ok" : "warn", x.in_sync ? "in sync" : x.copying ? "copying" : "behind"), fmt.n(x.seq_no), fmt.n(x.behind), fmt.n(x.copies), fmt.n(x.verifications), fmt.n(x.divergences)])) }));
      } else clear(body.repl);
    }
  }
  const table2 = (heads, rows) => h("div", { class: "tbl-wrap" }, h("table", { class: "t" }, h("thead", {}, h("tr", {}, heads.map((x) => h("th", {}, x)))), h("tbody", {}, rows.map((r) => h("tr", {}, r.map((c) => h("td", {}, c)))))));

  draw();
  refreshCluster();
  const offs = [bus.on("cluster", draw), bus.on("points", () => { if (!isCoordinator() && body.flow) storageFlow(body.flow, last() || {}, state.health, state.system); }), bus.on("health", () => { if (!isCoordinator()) draw(); })];
  return () => offs.forEach((f) => f());
}
