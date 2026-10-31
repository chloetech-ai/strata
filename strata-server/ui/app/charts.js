// Charts drawn in SVG (and a canvas for the heatmap): a time series with
// a crosshair that reads every series, a legend that hides one, a brush
// that zooms, a threshold that can be dragged; a stacked area; a latency
// heatmap; sparklines; a donut. One y axis each.

import { h, s, clear, fmt, showTip, hideTip, clamp } from "./core.js";

export const SERIES = ["var(--s1)", "var(--s2)", "var(--s3)", "var(--s4)", "var(--s5)", "var(--s6)", "var(--s7)", "var(--s8)"];
export const cssVar = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

export function niceStep(span, n) {
  const raw = span / Math.max(1, n);
  const mag = 10 ** Math.floor(Math.log10(raw || 1));
  const f = raw / mag;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10) * mag;
}

const TIME_STEPS = [5e3, 10e3, 15e3, 30e3, 60e3, 120e3, 300e3, 600e3, 900e3, 1800e3, 3600e3];
function timeTicks(t0, t1, width) {
  const want = Math.max(2, Math.floor(width / 90));
  const step = TIME_STEPS.find((st) => (t1 - t0) / st <= want) || 3600e3;
  const out = [];
  for (let t = Math.ceil(t0 / step) * step; t <= t1; t += step) out.push(t);
  return { ticks: out, step };
}

function bisect(points, t) {
  let lo = 0, hi = points.length - 1;
  if (hi < 0) return -1;
  while (lo < hi) { const mid = (lo + hi) >> 1; if (points[mid].t < t) lo = mid + 1; else hi = mid; }
  if (lo > 0 && Math.abs(points[lo - 1].t - t) < Math.abs(points[lo].t - t)) lo--;
  return lo;
}

/** A path through the points, broken where a value is missing. */
function linePath(pts) {
  let d = "", open = false;
  for (const [x, y] of pts) {
    if (y == null || Number.isNaN(y)) { open = false; continue; }
    d += (open ? "L" : "M") + x.toFixed(1) + "," + y.toFixed(1);
    open = true;
  }
  return d;
}

/**
 * A time series chart. opts: {height, series: [{name, color, get(p), area, dash}],
 * fmt(v), min, max, stack, band: {lo(p), hi(p), color}, threshold: {value, label, onDrag(v)},
 * domain: [t0, t1], onBrush(t0, t1) (null: reset), legend (default when 2+ series),
 * markers: [{t, label}]}. update(points) draws; points are [{t, ...}].
 */
export function timeChart(opts) {
  const height = opts.height || 180;
  const yfmt = opts.fmt || fmt.si;
  const hidden = new Set();
  const root = h("div", { class: "chart" });
  const legend = h("div", { class: "legend" });
  const svgEl = s("svg", { height, role: "img", "aria-label": opts.label || "chart" });
  root.append(...(opts.legend ?? opts.series.length > 1 ? [legend] : []), svgEl);
  let points = [], width = 0, threshold = opts.threshold ? opts.threshold.value : null;
  const M = { l: 46, r: 14, t: 8, b: 20 };

  function drawLegend() {
    clear(legend, opts.series.map((sr, i) => {
      const b = h("button", { type: "button", class: hidden.has(i) ? "off" : "", "aria-pressed": String(!hidden.has(i)), title: "Show or hide" },
        h("span", { class: "k" + (opts.stack || sr.area === "box" ? " box" : ""), style: { background: sr.color || SERIES[i] } }), sr.name);
      b.onclick = () => { if (hidden.has(i)) hidden.delete(i); else hidden.add(i); drawLegend(); draw(); };
      return b;
    }));
  }

  function draw() {
    width = svgEl.clientWidth || root.clientWidth || 600;
    svgEl.setAttribute("viewBox", `0 0 ${width} ${height}`);
    svgEl.replaceChildren();
    const W = width - M.l - M.r, H = height - M.t - M.b;
    const ps = points;
    const [t0, t1] = opts.domain ? opts.domain() : [ps[0]?.t ?? Date.now() - 300e3, ps[ps.length - 1]?.t ?? Date.now()];
    const span = Math.max(1, t1 - t0);
    const X = (t) => M.l + (t - t0) / span * W;
    const shown = opts.series.map((sr, i) => ({ ...sr, i, color: sr.color || SERIES[i] })).filter((sr) => !hidden.has(sr.i));
    // Values, stacked if asked.
    const vals = shown.map((sr) => ps.map((p) => { const v = sr.get(p); return v == null || !Number.isFinite(v) ? null : v; }));
    let lows = null;
    if (opts.stack) {
      lows = vals.map(() => new Array(ps.length).fill(0));
      for (let j = 0; j < ps.length; j++) { let acc = 0; for (let k = 0; k < vals.length; k++) { lows[k][j] = acc; acc += vals[k][j] || 0; vals[k][j] = acc; } }
    }
    let ymax = opts.max ?? 0;
    for (const v of vals) for (const x of v) if (x != null && x > ymax) ymax = x;
    if (opts.band) for (const p of ps) { const v = opts.band.hi(p); if (v != null && v > ymax) ymax = v; }
    if (threshold != null && opts.threshold?.showInScale !== false) ymax = Math.max(ymax, threshold * 1.15);
    if (!(ymax > 0)) ymax = 1;
    const step = niceStep(ymax, 4);
    ymax = Math.ceil(ymax / step) * step;
    const ymin = opts.min ?? 0;
    const Y = (v) => M.t + H - (v - ymin) / (ymax - ymin) * H;
    // Grid, axes.
    const g = s("g", { class: "grid" });
    const ax = s("g", { class: "axis" });
    for (let v = ymin; v <= ymax + 1e-9; v += step) {
      g.append(s("line", { x1: M.l, x2: width - M.r, y1: Y(v), y2: Y(v) }));
      ax.append(s("text", { x: M.l - 6, y: Y(v) + 3.5, "text-anchor": "end" }, yfmt(v)));
    }
    const { ticks } = timeTicks(t0, t1, W);
    for (const t of ticks) ax.append(s("text", { x: X(t), y: height - 5, "text-anchor": "middle" }, fmt.time(t, span < 600e3)));
    svgEl.append(g, ax, s("line", { class: "base", x1: M.l, x2: width - M.r, y1: Y(ymin), y2: Y(ymin) }));
    // Band (e.g. p50..p99).
    if (opts.band) {
      let d = "", seg = [];
      const flush = () => { if (seg.length > 1) { d += "M" + seg.map(([x, , hi]) => `${x.toFixed(1)},${hi.toFixed(1)}`).join("L") + "L" + seg.slice().reverse().map(([x, lo]) => `${x.toFixed(1)},${lo.toFixed(1)}`).join("L") + "Z"; } seg = []; };
      for (const p of ps) { const lo = opts.band.lo(p), hi = opts.band.hi(p); if (lo == null || hi == null) { flush(); continue; } seg.push([X(p.t), Y(lo), Y(hi)]); }
      flush();
      svgEl.append(s("path", { class: "band", d, style: { fill: opts.band.color || "var(--s1)" } }));
    }
    // Series.
    shown.forEach((sr, k) => {
      const pts = ps.map((p, j) => [X(p.t), vals[k][j] == null ? null : Y(vals[k][j])]);
      if (opts.stack) {
        let d = "";
        const top = [], bot = [];
        ps.forEach((p, j) => { top.push([X(p.t), Y(vals[k][j] ?? lows[k][j])]); bot.push([X(p.t), Y(lows[k][j])]); });
        if (top.length > 1) d = "M" + top.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join("L") + "L" + bot.reverse().map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join("L") + "Z";
        svgEl.append(s("path", { d, style: { fill: sr.color, opacity: 0.85, stroke: "var(--surface)", strokeWidth: 1 } }));
      } else {
        if (sr.area) {
          // One filled shape per run of values, so a gap stays empty.
          let d = "", run = [];
          const flush = () => { if (run.length > 1) d += "M" + run.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join("L") + `L${run[run.length - 1][0].toFixed(1)},${Y(ymin)}L${run[0][0].toFixed(1)},${Y(ymin)}Z`; run = []; };
          for (const pt of pts) { if (pt[1] == null) flush(); else run.push(pt); }
          flush();
          if (d) svgEl.append(s("path", { class: "area", d, style: { fill: sr.color } }));
        }
        svgEl.append(s("path", { class: "line", d: linePath(pts), style: { stroke: sr.color, strokeDasharray: sr.dash || null } }));
      }
    });
    // Markers (events) on the time axis.
    for (const m of opts.markers?.() || []) {
      if (m.t < t0 || m.t > t1) continue;
      const x = X(m.t);
      svgEl.append(s("line", { class: "ann", x1: x, x2: x, y1: M.t, y2: Y(ymin) }), s("circle", { class: "ann-dot", cx: x, cy: M.t + 3, r: 3 }, s("title", {}, m.label)));
    }
    // Threshold.
    if (threshold != null) {
      const y = Y(clamp(threshold, ymin, ymax));
      const lbl = s("text", { class: "thr-lbl", x: width - M.r - 4, y: y - 5, "text-anchor": "end" }, (opts.threshold.label || "threshold") + " " + yfmt(threshold));
      const line = s("line", { class: "thr", x1: M.l, x2: width - M.r, y1: y, y2: y });
      svgEl.append(line, lbl);
      if (opts.threshold.onDrag) {
        const handle = s("rect", { class: "thr-h", x: M.l - 8, y: y - 6, width: 16, height: 12, rx: 3, tabindex: 0, role: "slider", "aria-label": "threshold", "aria-valuenow": threshold });
        const hit = s("rect", { x: M.l, y: y - 6, width: W, height: 12, fill: "transparent", style: { cursor: "ns-resize" } });
        svgEl.append(hit, handle);
        const start = (ev) => {
          ev.preventDefault(); ev.stopPropagation();
          const r = svgEl.getBoundingClientRect();
          const move = (e) => {
            const yy = clamp(e.clientY - r.top, M.t, M.t + H);
            let v = ymin + (M.t + H - yy) / H * (ymax - ymin);
            const st = niceStep(ymax, 40); v = Math.round(v / st) * st;
            threshold = v; const py = Y(v);
            line.setAttribute("y1", py); line.setAttribute("y2", py); handle.setAttribute("y", py - 6); hit.setAttribute("y", py - 6);
            lbl.setAttribute("y", py - 5); lbl.textContent = (opts.threshold.label || "threshold") + " " + yfmt(v);
            opts.threshold.onMove?.(v);
          };
          const up = () => { removeEventListener("pointermove", move); removeEventListener("pointerup", up); opts.threshold.onDrag(threshold); draw(); };
          addEventListener("pointermove", move); addEventListener("pointerup", up);
        };
        handle.addEventListener("pointerdown", start); hit.addEventListener("pointerdown", start);
        handle.addEventListener("keydown", (e) => {
          const st = niceStep(ymax, 40);
          if (e.key === "ArrowUp" || e.key === "ArrowDown") { e.preventDefault(); threshold += e.key === "ArrowUp" ? st : -st; opts.threshold.onDrag(threshold); draw(); svgEl.querySelector(".thr-h")?.focus(); }
        });
      }
    }
    if (!ps.length) {
      svgEl.append(s("text", { x: M.l + W / 2, y: M.t + H / 2, "text-anchor": "middle", class: "axis" }, "waiting for points…"));
      return;
    }
    // Crosshair, tooltip, brush.
    const xh = s("line", { class: "xhair", y1: M.t, y2: M.t + H, style: { display: "none" } });
    const dots = s("g");
    const brush = s("rect", { class: "brush", y: M.t, height: H, style: { display: "none" } });
    const over = s("rect", { x: M.l, y: M.t, width: W, height: H, fill: "transparent", style: { cursor: opts.onBrush ? "crosshair" : "default" } });
    svgEl.append(brush, xh, dots, over);
    let b0 = null;
    const tAt = (cx) => t0 + clamp((cx - M.l) / W, 0, 1) * span;
    over.addEventListener("pointermove", (e) => {
      const r = svgEl.getBoundingClientRect();
      const cx = (e.clientX - r.left) * (width / r.width);
      if (b0 != null) { const a = Math.min(b0, cx), b = Math.max(b0, cx); brush.setAttribute("x", a); brush.setAttribute("width", b - a); brush.style.display = ""; }
      const j = bisect(ps, tAt(cx));
      if (j < 0) return;
      const p = ps[j], x = X(p.t);
      xh.setAttribute("x1", x); xh.setAttribute("x2", x); xh.style.display = "";
      dots.replaceChildren();
      const rows = [];
      shown.forEach((sr, k) => {
        const v = vals[k][j];
        const raw = sr.get(p);
        if (v != null) dots.append(s("circle", { class: "dot-hl", cx: x, cy: Y(v), r: 4, style: { fill: sr.color } }));
        rows.push(h("div", { class: "r" }, h("span", { class: "k", style: { background: sr.color } }), h("b", {}, raw == null ? "–" : (sr.fmt || yfmt)(raw)), h("span", { class: "n" }, sr.name)));
      });
      if (opts.band) rows.push(h("div", { class: "r" }, h("span", { class: "k box", style: { background: opts.band.color || "var(--s1)", opacity: 0.4 } }), h("b", {}, `${yfmt(opts.band.lo(p))} – ${yfmt(opts.band.hi(p))}`), h("span", { class: "n" }, opts.band.name || "range")));
      showTip(e.clientX, e.clientY, [h("div", { class: "t" }, fmt.datetime(p.t)), ...rows, ...(opts.tipExtra ? [opts.tipExtra(p)] : [])]);
    });
    over.addEventListener("pointerleave", () => { if (b0 == null) { xh.style.display = "none"; dots.replaceChildren(); hideTip(); } });
    if (opts.onBrush) {
      over.addEventListener("pointerdown", (e) => { const r = svgEl.getBoundingClientRect(); b0 = (e.clientX - r.left) * (width / r.width); over.setPointerCapture(e.pointerId); });
      over.addEventListener("pointerup", (e) => {
        if (b0 == null) return;
        const r = svgEl.getBoundingClientRect(); const b1 = (e.clientX - r.left) * (width / r.width);
        const a = Math.min(b0, b1), b = Math.max(b0, b1); b0 = null; brush.style.display = "none";
        if (b - a > 8) { hideTip(); opts.onBrush(tAt(a), tAt(b)); }
      });
      over.addEventListener("dblclick", () => opts.onBrush(null));
    }
  }

  if (opts.legend ?? opts.series.length > 1) drawLegend();
  const ro = new ResizeObserver(() => { if (root.isConnected && Math.abs((svgEl.clientWidth || 0) - width) > 2) draw(); });
  ro.observe(root);
  root.update = (ps) => { points = ps || []; draw(); };
  root.setThreshold = (v) => { threshold = v; draw(); };
  root.redraw = draw;
  root.hold = (on) => root.classList.toggle("hold", on);
  return root;
}

/** A sparkline: values in a small box, the last value marked. */
export function sparkline(values, { color = "var(--s1)", width = 120, height = 36, area = true, cls = "spark" } = {}) {
  const v = values.map((x) => (x == null || !Number.isFinite(x) ? null : x));
  const nums = v.filter((x) => x != null);
  const svgEl = s("svg", { class: cls, viewBox: `0 0 ${width} ${height}`, preserveAspectRatio: "none", "aria-hidden": "true" });
  if (nums.length < 2) return svgEl;
  const max = Math.max(...nums), min = Math.min(0, ...nums);
  const X = (i) => i / (v.length - 1) * width, Y = (x) => height - 3 - (x - min) / ((max - min) || 1) * (height - 6);
  const pts = v.map((x, i) => [X(i), x == null ? null : Y(x)]);
  const d = linePath(pts);
  if (area) svgEl.append(s("path", { d: d + `L${width},${height}L0,${height}Z`, style: { fill: color, opacity: 0.14 } }));
  svgEl.append(s("path", { d, style: { fill: "none", stroke: color, strokeWidth: 1.6, vectorEffect: "non-scaling-stroke", strokeLinejoin: "round" } }));
  return svgEl;
}

/**
 * The latency histogram over time, as a heatmap: a column per point, a
 * row per bucket (bounds in seconds plus "over"), darker = more queries.
 */
export function heatmap(opts) {
  const height = opts.height || 190;
  const root = h("div", { class: "chart" });
  const cv = h("canvas", { style: { width: "100%", height: height + "px", display: "block" } });
  const ax = s("svg", { height: 0, style: { position: "absolute", left: 0, top: 0, width: "100%", height: height + "px", pointerEvents: "none", overflow: "visible" } });
  root.style.height = height + "px";
  root.append(cv, ax);
  let points = [];
  const bounds = opts.bounds; // seconds
  const labels = [...bounds.map((b) => fmt.ms(b * 1000)), "more"];
  const M = { l: 52, r: 14, t: 6, b: 20 };
  function draw() {
    const w = root.clientWidth || 600, dpr = devicePixelRatio || 1;
    cv.width = w * dpr; cv.height = height * dpr;
    const c = cv.getContext("2d"); c.scale(dpr, dpr); c.clearRect(0, 0, w, height);
    ax.replaceChildren(); ax.setAttribute("viewBox", `0 0 ${w} ${height}`);
    const W = w - M.l - M.r, H = height - M.t - M.b;
    const rows = labels.length, rh = H / rows;
    const [t0, t1] = opts.domain ? opts.domain() : [points[0]?.t ?? 0, points[points.length - 1]?.t ?? 1];
    const span = Math.max(1, t1 - t0);
    const ramp = [1, 2, 3, 4, 5, 6, 7].map((i) => cssVar("--seq-" + i));
    const empty = cssVar("--sunken");
    c.fillStyle = empty; c.fillRect(M.l, M.t, W, H);
    let max = 0;
    for (const p of points) for (const n of p.latency_hist || []) if (n > max) max = n;
    const cw = Math.max(1.5, W / Math.max(1, span / (opts.interval || 2000)));
    for (const p of points) {
      if (p.t < t0 || p.t > t1) continue;
      const x = M.l + (p.t - t0) / span * W - cw / 2;
      (p.latency_hist || []).forEach((n, r) => {
        if (!n) return;
        const k = Math.min(6, Math.floor(Math.log1p(n) / Math.log1p(max || 1) * 6.999));
        c.fillStyle = ramp[k];
        c.fillRect(x, M.t + H - (r + 1) * rh + 1, Math.max(1, cw - 0.5), rh - 1);
      });
    }
    for (let r = 0; r < rows; r += rows > 10 ? 2 : 1) ax.append(s("text", { class: "axis", x: M.l - 6, y: M.t + H - r * rh - rh / 2 + 3.5, "text-anchor": "end" }, r === rows - 1 ? "more" : "≤ " + labels[r]));
    const { ticks } = timeTicks(t0, t1, W);
    for (const t of ticks) ax.append(s("text", { class: "axis", x: M.l + (t - t0) / span * W, y: height - 5, "text-anchor": "middle" }, fmt.time(t, span < 600e3)));
    root.geom = { W, H, rh, t0, span, rows };
  }
  cv.addEventListener("pointermove", (e) => {
    const g = root.geom; if (!g) return;
    const r = cv.getBoundingClientRect();
    const x = e.clientX - r.left, y = e.clientY - r.top;
    if (x < M.l || y < M.t || y > M.t + g.H) { hideTip(); return; }
    const t = g.t0 + (x - M.l) / g.W * g.span;
    const j = bisect(points, t); if (j < 0) return;
    const row = Math.floor((M.t + g.H - y) / g.rh);
    const p = points[j]; const n = p.latency_hist?.[row] ?? 0;
    const lo = row === 0 ? 0 : bounds[row - 1] * 1000, hi = bounds[row] != null ? bounds[row] * 1000 : null;
    const total = (p.latency_hist || []).reduce((a, b) => a + b, 0);
    showTip(e.clientX, e.clientY, [h("div", { class: "t" }, fmt.datetime(p.t)), h("div", { class: "r" }, h("b", {}, fmt.n(n) + " queries"), h("span", { class: "n" }, hi == null ? `over ${fmt.ms(lo)}` : `${fmt.ms(lo)} – ${fmt.ms(hi)}`)), h("div", { class: "r" }, h("span", { class: "n", style: { marginLeft: 0 } }, total ? fmt.pct(n / total) + " of the interval's " + fmt.n(total) : ""))]);
  });
  cv.addEventListener("pointerleave", hideTip);
  const ro = new ResizeObserver(() => { if (root.isConnected) draw(); });
  ro.observe(root);
  root.update = (ps) => { points = ps || []; draw(); };
  return root;
}

/** A donut of shares: items [{name, value, color}]; center text. */
export function donut(items, { size = 150, thick = 20, center, sub, fmtv = fmt.si } = {}) {
  const total = items.reduce((a, b) => a + (b.value || 0), 0);
  const r = size / 2, ri = r - thick;
  const svgEl = s("svg", { viewBox: `0 0 ${size} ${size}`, width: size, height: size, role: "img", "aria-label": "shares" });
  if (!total) {
    svgEl.append(s("circle", { cx: r, cy: r, r: r - thick / 2, style: { fill: "none", stroke: "var(--surface-3)", strokeWidth: thick } }));
  } else {
    let a0 = -Math.PI / 2;
    const gap = items.filter((x) => x.value > 0).length > 1 ? 0.025 : 0;
    items.forEach((it, i) => {
      if (!it.value) return;
      const a1 = a0 + it.value / total * Math.PI * 2;
      const s0 = a0 + gap / 2, s1 = Math.max(s0 + 0.001, a1 - gap / 2);
      const large = s1 - s0 > Math.PI ? 1 : 0;
      const p = (ang, rr) => `${(r + rr * Math.cos(ang)).toFixed(2)},${(r + rr * Math.sin(ang)).toFixed(2)}`;
      const d = `M${p(s0, r)}A${r},${r} 0 ${large} 1 ${p(s1, r)}L${p(s1, ri)}A${ri},${ri} 0 ${large} 0 ${p(s0, ri)}Z`;
      const path = s("path", { d, style: { fill: it.color || SERIES[i % 8], cursor: "default" }, tabindex: 0 });
      const tip = (e) => showTip(e.clientX ?? 0, e.clientY ?? 0, [h("div", { class: "r" }, h("span", { class: "k box", style: { background: it.color || SERIES[i % 8] } }), h("b", {}, fmtv(it.value)), h("span", { class: "n" }, `${it.name} · ${fmt.pct(it.value / total)}`))]);
      path.addEventListener("pointermove", tip); path.addEventListener("pointerleave", hideTip);
      path.addEventListener("mouseenter", () => path.style.opacity = 0.8); path.addEventListener("mouseleave", () => path.style.opacity = 1);
      svgEl.append(path);
      a0 = a1;
    });
  }
  if (center != null) svgEl.append(s("text", { x: r, y: r + (sub ? 2 : 6), "text-anchor": "middle", style: { fill: "var(--text)", fontSize: "20px", fontWeight: 650 } }, center));
  if (sub) svgEl.append(s("text", { x: r, y: r + 18, "text-anchor": "middle", style: { fill: "var(--muted)", fontSize: "11px" } }, sub));
  return svgEl;
}

/** A list of bars with labels and values: items [{name, value, color, sub}]. */
export function barList(items, { fmtv = fmt.si, max } = {}) {
  const m = max ?? Math.max(1e-9, ...items.map((i) => i.value || 0));
  return h("div", { class: "stack", style: { gap: "8px" } }, items.map((it, i) => h("div", {},
    h("div", { class: "row nw between small" }, h("span", { class: "ellipsis dim", title: it.name }, it.name), h("b", { class: "num" }, fmtv(it.value))),
    h("div", { class: "meter", style: { marginTop: "3px" } }, h("i", { style: { width: clamp((it.value || 0) / m * 100, 0, 100) + "%", background: it.color || SERIES[i % 8] } })))));
}

/** A semicircle gauge of a ratio 0..1, with warn and bad thresholds. */
export function gauge(v, { size = 120, warn = 0.75, bad = 0.9, label } = {}) {
  const r = size / 2 - 8, cx = size / 2, cy = size / 2 + 4;
  const arc = (a0, a1) => { const p = (a) => `${(cx + r * Math.cos(a)).toFixed(2)},${(cy + r * Math.sin(a)).toFixed(2)}`; return `M${p(a0)}A${r},${r} 0 ${a1 - a0 > Math.PI ? 1 : 0} 1 ${p(a1)}`; };
  const x = clamp(v ?? 0, 0, 1);
  const color = x >= bad ? "var(--crit)" : x >= warn ? "var(--warn)" : "var(--s3)";
  return s("svg", { viewBox: `0 0 ${size} ${size / 2 + 16}`, width: size, role: "img", "aria-label": label || "gauge" },
    s("path", { d: arc(Math.PI, 2 * Math.PI), style: { fill: "none", stroke: "var(--surface-3)", strokeWidth: 10, strokeLinecap: "round" } }),
    x > 0 ? s("path", { d: arc(Math.PI, Math.PI + Math.max(0.02, x) * Math.PI), style: { fill: "none", stroke: color, strokeWidth: 10, strokeLinecap: "round" } }) : null,
    s("text", { x: cx, y: cy - 4, "text-anchor": "middle", style: { fill: "var(--text)", fontSize: "18px", fontWeight: 650 } }, v == null ? "–" : fmt.pct(x, 0)),
    label ? s("text", { x: cx, y: cy + 12, "text-anchor": "middle", style: { fill: "var(--muted)", fontSize: "10.5px" } }, label) : null);
}

/**
 * A line chart over a numeric x (positions, days, characters typed).
 * opts: {height, series: [{name, color, points: [[x, y, extra]], dash, area}],
 * xfmt(x), yfmt(y), xticks: [x] (default: nice steps), min, max,
 * marks: [{x, label}] (dashed vertical lines), tipTitle(x), tipExtra(x)}.
 * Hovering reads every series at the nearest x. update(series) redraws.
 */
export function xyChart(opts) {
  const height = opts.height || 200;
  const yfmt = opts.yfmt || fmt.si, xfmt = opts.xfmt || ((x) => fmt.si(x));
  const hidden = new Set();
  let series = opts.series || [];
  const root = h("div", { class: "chart" });
  const legend = h("div", { class: "legend" });
  const svgEl = s("svg", { height, role: "img", "aria-label": opts.label || "chart" });
  root.append(legend, svgEl);
  const M = { l: 46, r: 16, t: 10, b: 22, ...(opts.margin || {}) };
  let width = 0;
  function drawLegend() {
    legend.style.display = series.length > 1 ? "" : "none";
    clear(legend, series.map((sr, i) => {
      const b = h("button", { type: "button", class: hidden.has(i) ? "off" : "", "aria-pressed": String(!hidden.has(i)), title: "Show or hide" },
        h("span", { class: "k", style: { background: sr.color || SERIES[i] } }), sr.name);
      b.onclick = () => { if (hidden.has(i)) hidden.delete(i); else hidden.add(i); drawLegend(); draw(); };
      return b;
    }));
  }
  function draw() {
    width = svgEl.clientWidth || root.clientWidth || 600;
    svgEl.setAttribute("viewBox", `0 0 ${width} ${height}`);
    svgEl.replaceChildren();
    const W = width - M.l - M.r, H = height - M.t - M.b;
    const shown = series.map((sr, i) => ({ ...sr, i, color: sr.color || SERIES[i] })).filter((sr) => !hidden.has(sr.i));
    const xs = [...new Set(shown.flatMap((sr) => sr.points.map((p) => p[0])))].sort((a, b) => a - b);
    if (!xs.length) { svgEl.append(s("text", { x: M.l + W / 2, y: M.t + H / 2, "text-anchor": "middle", class: "axis" }, opts.emptyText || "nothing to draw")); return; }
    const x0 = opts.xmin ?? xs[0], x1 = opts.xmax ?? xs[xs.length - 1];
    const X = (x) => M.l + (x1 === x0 ? W / 2 : (x - x0) / (x1 - x0) * W);
    let ymax = opts.max ?? 0, ymin = opts.min ?? 0;
    for (const sr of shown) for (const p of sr.points) { if (p[1] != null && p[1] > ymax && opts.max == null) ymax = p[1]; if (p[1] != null && p[1] < ymin && opts.min == null) ymin = p[1]; }
    if (!(ymax > ymin)) ymax = ymin + 1;
    const step = niceStep(ymax - ymin, 4);
    if (opts.max == null) ymax = Math.ceil(ymax / step) * step;
    const Y = (v) => M.t + H - (v - ymin) / (ymax - ymin) * H;
    const g = s("g", { class: "grid" }), ax = s("g", { class: "axis" });
    for (let v = Math.ceil(ymin / step) * step; v <= ymax + 1e-9; v += step) {
      g.append(s("line", { x1: M.l, x2: width - M.r, y1: Y(v), y2: Y(v) }));
      ax.append(s("text", { x: M.l - 6, y: Y(v) + 3.5, "text-anchor": "end" }, yfmt(v)));
    }
    const want = Math.max(2, Math.floor(W / 70));
    const ticks = opts.xticks || (xs.length <= want ? xs : (() => { const st = niceStep(x1 - x0, want); const out = []; for (let x = Math.ceil(x0 / st) * st; x <= x1 + 1e-9; x += st) out.push(x); return out; })());
    for (const x of ticks) ax.append(s("text", { x: X(x), y: height - 6, "text-anchor": "middle" }, xfmt(x)));
    svgEl.append(g, ax, s("line", { class: "base", x1: M.l, x2: width - M.r, y1: Y(Math.max(ymin, 0)), y2: Y(Math.max(ymin, 0)) }));
    for (const m of opts.marks || []) {
      const x = X(m.x);
      svgEl.append(s("line", { class: "ann", x1: x, x2: x, y1: M.t, y2: M.t + H }), s("text", { class: "axis", x: x + 4, y: M.t + 10 }, m.label));
    }
    for (const sr of shown) {
      const pts = sr.points.map((p) => [X(p[0]), p[1] == null ? null : Y(p[1])]);
      if (sr.area) {
        const run = pts.filter((p) => p[1] != null);
        if (run.length > 1) svgEl.append(s("path", { class: "area", d: "M" + run.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join("L") + `L${run[run.length - 1][0].toFixed(1)},${Y(Math.max(ymin, 0))}L${run[0][0].toFixed(1)},${Y(Math.max(ymin, 0))}Z`, style: { fill: sr.color } }));
      }
      svgEl.append(s("path", { class: "line", d: linePath(pts), style: { stroke: sr.color, strokeDasharray: sr.dash || null } }));
      if (sr.points.length <= 40) for (const [x, y] of pts) if (y != null) svgEl.append(s("circle", { cx: x, cy: y, r: 3, style: { fill: sr.color, stroke: "var(--surface)", strokeWidth: 1.5 } }));
    }
    const xh = s("line", { class: "xhair", y1: M.t, y2: M.t + H, style: { display: "none" } });
    const dots = s("g");
    const over = s("rect", { x: M.l, y: M.t, width: W, height: H, fill: "transparent" });
    svgEl.append(xh, dots, over);
    over.addEventListener("pointermove", (e) => {
      const r = svgEl.getBoundingClientRect();
      const cx = (e.clientX - r.left) * (width / r.width);
      let best = xs[0];
      for (const x of xs) if (Math.abs(X(x) - cx) < Math.abs(X(best) - cx)) best = x;
      xh.setAttribute("x1", X(best)); xh.setAttribute("x2", X(best)); xh.style.display = "";
      dots.replaceChildren();
      const rows = [];
      for (const sr of shown) {
        const p = sr.points.find((q) => q[0] === best);
        if (p && p[1] != null) dots.append(s("circle", { class: "dot-hl", cx: X(best), cy: Y(p[1]), r: 4.5, style: { fill: sr.color } }));
        rows.push(h("div", { class: "r" }, h("span", { class: "k", style: { background: sr.color } }), h("b", {}, p && p[1] != null ? yfmt(p[1]) : "–"), h("span", { class: "n" }, sr.name + (p && p[2] != null ? ` · ${p[2]}` : ""))));
      }
      showTip(e.clientX, e.clientY, [h("div", { class: "t" }, opts.tipTitle ? opts.tipTitle(best) : xfmt(best)), ...rows, ...(opts.tipExtra ? [opts.tipExtra(best)] : [])]);
    });
    over.addEventListener("pointerleave", () => { xh.style.display = "none"; dots.replaceChildren(); hideTip(); });
  }
  drawLegend();
  const ro = new ResizeObserver(() => { if (root.isConnected && Math.abs((svgEl.clientWidth || 0) - width) > 2) draw(); });
  ro.observe(root);
  root.update = (sr) => { series = sr || []; drawLegend(); draw(); };
  setTimeout(draw, 0);
  return root;
}
