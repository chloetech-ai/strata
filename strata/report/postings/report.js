(function () {
  "use strict";
  const NS = "http://www.w3.org/2000/svg";
  const $ = (s) => document.querySelector(s);

  function s(tag, attrs, parent, text) {
    const e = document.createElementNS(NS, tag);
    for (const k in attrs || {}) e.setAttribute(k, attrs[k]);
    if (text !== undefined) e.textContent = text;
    if (parent) parent.appendChild(e);
    return e;
  }
  function h(tag, attrs, parent, text) {
    const e = document.createElement(tag);
    for (const k in attrs || {}) e.setAttribute(k, attrs[k]);
    if (text !== undefined) e.textContent = text;
    if (parent) parent.appendChild(e);
    return e;
  }
  const fmt = (v, d) => (v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(d === undefined ? 1 : Math.max(0, d - 1)) : v.toFixed(d === undefined ? 2 : d));
  const ms = (v) => (v < 0.1 ? v.toFixed(3) : v < 10 ? v.toFixed(2) : v.toFixed(1));
  const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

  // ---------------------------------------------------------------- diagrams
  function blockDiagram() {
    const g = $("#blk-docs");
    const docs = [3, 7, 8, 9, 15, 16, 22, 40];
    const gaps = [3, 3, 0, 0, 5, 0, 5, 17];
    docs.forEach((d, i) => {
      const x = 128 + i * 60;
      s("rect", { x, y: 16, width: 52, height: 28, rx: 4, class: "box" }, g);
      s("text", { x: x + 26, y: 35, class: "tm", "text-anchor": "middle" }, g, d);
      s("rect", { x, y: 68, width: 52, height: 28, rx: 4, class: "box-acc" }, g);
      s("text", { x: x + 26, y: 87, class: "tm", "text-anchor": "middle" }, g, gaps[i]);
    });
    s("text", { x: 128 + 8 * 60 + 6, y: 35, class: "tl" }, g, "… 128 per block");
    s("text", { x: 128 + 8 * 60 + 6, y: 87, class: "tl" }, g, "max gap 17 → b = 5");
  }

  function efDiagram() {
    const vals = [3, 4, 7, 13, 14, 15, 21, 31];
    const gv = $("#ef-values");
    vals.forEach((v, i) => {
      const x = 130 + i * 76;
      s("rect", { x, y: 10, width: 64, height: 26, rx: 4, class: "box" }, gv);
      s("text", { x: x + 32, y: 28, class: "tm", "text-anchor": "middle" }, gv, v);
      const hi = (v >> 2).toString(2).padStart(3, "0");
      const lo = (v & 3).toString(2).padStart(2, "0");
      s("text", { x: x + 32, y: 62, class: "tm", "text-anchor": "middle" }, gv, hi + "│" + lo);
    });
    const groups = [[1, 0], [1, 1, 0], [0], [1, 1, 1, 0], [0], [1, 0], [0], [1, 0]];
    const gh = $("#ef-high");
    let x = 130;
    groups.forEach((bits, b) => {
      const x0 = x;
      bits.forEach((bit) => {
        s("rect", { x, y: 112, width: 24, height: 26, rx: 3, class: bit ? "box-acc" : "box" }, gh);
        s("text", { x: x + 12, y: 130, class: "tm", "text-anchor": "middle" }, gh, bit);
        x += 26;
      });
      s("text", { x: (x0 + x - 2) / 2, y: 156, class: "ts", "text-anchor": "middle" }, gh, "h=" + b);
      x += 8;
    });
    const gl = $("#ef-low");
    vals.forEach((v, i) => {
      const xx = 130 + i * 58;
      s("rect", { x: xx, y: 196, width: 50, height: 26, rx: 3, class: "box" }, gl);
      s("text", { x: xx + 25, y: 214, class: "tm", "text-anchor": "middle" }, gl, (v & 3).toString(2).padStart(2, "0"));
    });
  }

  function pefDiagram() {
    const g = $("#pef-parts");
    const parts = [
      { a: 40, b: 120, kind: "run", cls: "box-acc", density: 1 },
      { a: 120, b: 330, kind: "Elias-Fano", cls: "box", density: 0.12 },
      { a: 330, b: 455, kind: "bitvector", cls: "box-warm", density: 0.55 },
      { a: 455, b: 625, kind: "Elias-Fano", cls: "box", density: 0.08 },
      { a: 625, b: 680, kind: "run", cls: "box-acc", density: 1 },
      { a: 680, b: 740, kind: "EF", cls: "box", density: 0.15 },
    ];
    let seed = 7;
    const rnd = () => ((seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648);
    parts.forEach((p) => {
      s("rect", { x: p.a + 1, y: 52, width: p.b - p.a - 2, height: 66, rx: 4, class: p.cls }, g);
      s("text", { x: (p.a + p.b) / 2, y: 70, class: "tb", "text-anchor": "middle" }, g, p.kind);
      for (let x = p.a + 5; x < p.b - 3; x += 4) {
        if (p.density >= 1 || rnd() < p.density) s("circle", { cx: x, cy: 100, r: 1.7, class: "dot" }, g);
      }
      s("rect", { x: p.b - 44, y: 20, width: 42, height: 22, rx: 3, class: "box" }, g);
      s("text", { x: p.b - 23, y: 35, class: "ts", "text-anchor": "middle" }, g, "last");
      s("line", { x1: p.b - 2, y1: 42, x2: p.b - 2, y2: 52, class: "ln" }, g);
    });
    s("text", { x: 40, y: 12, class: "ts" }, g, "upper level: last docID + descriptor per partition (64 bits)");
  }

  function roaringDiagram() {
    const g = $("#roar-keys");
    const items = [
      { key: 0, title: "array · 3 values", body: "[17, 902, 4410]", size: "6 bytes", cls: "box" },
      { key: 1, title: "bitmap · 31,207 values", body: "1024 × 64-bit words", size: "8 KiB", cls: "box-acc" },
      { key: 5, title: "run · 2 runs", body: "(100,2999) (8000,199)", size: "10 bytes", cls: "box-warm" },
      { key: 9, title: "array · 1 value", body: "[65530]", size: "2 bytes", cls: "box" },
    ];
    s("text", { x: 20, y: 86, class: "tl" }, g, "keys");
    items.forEach((it, i) => {
      const kx = 70 + i * 62;
      s("rect", { x: kx, y: 66, width: 54, height: 30, rx: 4, class: "box-acc" }, g);
      s("text", { x: kx + 27, y: 86, class: "tm", "text-anchor": "middle" }, g, it.key);
      const cx = 14 + i * 186;
      s("rect", { x: cx, y: 136, width: 176, height: 86, rx: 6, class: it.cls }, g);
      s("text", { x: cx + 10, y: 158, class: "tb" }, g, it.title);
      s("text", { x: cx + 10, y: 182, class: "tm" }, g, it.body);
      s("text", { x: cx + 10, y: 208, class: "ts" }, g, it.size);
      s("path", { d: `M${kx + 27} 96 C ${kx + 27} 118, ${cx + 88} 112, ${cx + 88} 134`, class: "ln", "marker-end": "url(#ar2)" }, g);
    });
  }

  function trieDiagram() {
    const g = $("#trie-tree");
    const H = 4, W = 400, X0 = 14;
    const ys = [28, 86, 144, 202, 262];
    const xOf = (p, d) => X0 + ((p + 0.5) * 2 ** (H - d)) / 16 * W;
    const nodes = [
      { p: 0, d: 0, code: "11" },
      { p: 0, d: 1, code: "10" }, { p: 1, d: 1, code: "11" },
      { p: 0, d: 2, code: "01" }, { p: 2, d: 2, code: "10" }, { p: 3, d: 2, code: "10" },
      { p: 1, d: 3, code: "11" }, { p: 4, d: 3, code: "01" }, { p: 6, d: 3, code: "11" },
    ];
    const leaves = [2, 3, 9, 12, 13];
    const present = new Set(nodes.map((n) => n.d + ":" + n.p).concat(leaves.map((v) => "4:" + v)));
    const edges = [];
    nodes.forEach((n) => {
      [0, 1].forEach((bit) => {
        const c = n.p * 2 + bit;
        if (present.has(n.d + 1 + ":" + c)) edges.push([n, { p: c, d: n.d + 1 }, bit]);
      });
    });
    edges.forEach(([a, b, bit]) => {
      const x1 = xOf(a.p, a.d), y1 = ys[a.d], x2 = xOf(b.p, b.d), y2 = ys[b.d];
      s("line", { x1, y1, x2, y2, class: "ln" }, g);
      s("text", { x: (x1 + x2) / 2 + (bit ? 6 : -12), y: (y1 + y2) / 2, class: "ts" }, g, bit);
    });
    nodes.forEach((n) => {
      const x = xOf(n.p, n.d), y = ys[n.d];
      s("rect", { x: x - 17, y: y - 12, width: 34, height: 24, rx: 12, class: n.d === 2 && n.p === 2 ? "box-warm" : "box-acc" }, g);
      s("text", { x, y: y + 4, class: "tm", "text-anchor": "middle" }, g, n.code);
    });
    leaves.forEach((v) => {
      const x = xOf(v, 4), y = ys[4];
      s("rect", { x: x - 11, y: y - 11, width: 22, height: 22, rx: 3, class: "box" }, g);
      s("text", { x, y: y + 4, class: "tm", "text-anchor": "middle" }, g, v);
    });
    const gl = $("#trie-levels");
    const levels = [["11"], ["10", "11"], ["01", "10", "10"], ["11", "01", "11"]];
    s("text", { x: 470, y: 14, class: "ts" }, gl, "level bitvectors (rank-indexed)");
    levels.forEach((lv, d) => {
      s("text", { x: 470, y: ys[d] + 4, class: "tl" }, gl, "B" + "₀₁₂₃"[d]);
      lv.forEach((code, i) => {
        const x = 500 + i * 62;
        s("rect", { x, y: ys[d] - 12, width: 54, height: 24, rx: 4, class: d === 2 && i === 1 ? "box-warm" : "box" }, gl);
        s("text", { x: x + 27, y: ys[d] + 4, class: "tm", "text-anchor": "middle" }, gl, code);
      });
    });
    s("text", { x: 470, y: 250, class: "ts" }, gl, "18 bits for 5 docIDs; leaves are implicit.");
    s("text", { x: 470, y: 268, class: "ts" }, gl, "left(i) = rank₁(B_ℓ, 2i),  right(i) = rank₁(B_ℓ, 2i+1)");
  }

  function simdDiagram() {
    const g = $("#simd-grid");
    const a = [12, 15, 20, 31];
    const b = [9, 12, 22, 31];
    const rows = [0, 1, 2, 3].map((r) => b.map((_, j) => b[(j + r) % 4]));
    const X = 120, CW = 50;
    s("text", { x: 10, y: 33, class: "tb" }, g, "a block");
    a.forEach((v, j) => {
      s("rect", { x: X + j * (CW + 6), y: 12, width: CW, height: 30, rx: 4, class: "box-acc" }, g);
      s("text", { x: X + j * (CW + 6) + CW / 2, y: 32, class: "tm", "text-anchor": "middle" }, g, v);
    });
    const masks = [];
    rows.forEach((row, r) => {
      const y = 62 + r * 40;
      s("text", { x: 10, y: y + 20, class: "tl" }, g, r === 0 ? "b block" : "b rotated " + r);
      let m = 0;
      row.forEach((v, j) => {
        const hit = v === a[j];
        if (hit) m |= 1 << j;
        s("rect", { x: X + j * (CW + 6), y, width: CW, height: 30, rx: 4, class: hit ? "box-warm" : "box" }, g);
        s("text", { x: X + j * (CW + 6) + CW / 2, y: y + 20, class: "tm", "text-anchor": "middle" }, g, v);
      });
      masks.push(m);
      s("text", { x: X + 4 * (CW + 6) + 10, y: y + 20, class: "tm" }, g, "mask " + m.toString(2).padStart(4, "0") + "₂");
    });
    const all = masks.reduce((x, y) => x | y, 0);
    const out = a.filter((_, j) => all & (1 << j));
    const ox = 470;
    s("text", { x: ox, y: 80, class: "tl" }, g, "OR of masks = " + all.toString(2).padStart(4, "0") + "₂");
    s("text", { x: ox, y: 104, class: "tl" }, g, "pshufb with table[" + all + "]");
    s("text", { x: ox, y: 140, class: "tb" }, g, "output");
    out.forEach((v, j) => {
      s("rect", { x: ox + 56 + j * 56, y: 120, width: 50, height: 30, rx: 4, class: "box-warm" }, g);
      s("text", { x: ox + 81 + j * 56, y: 140, class: "tm", "text-anchor": "middle" }, g, v);
    });
    s("text", { x: ox, y: 182, class: "ts" }, g, "a₃ = 31 ≤ b₃ = 31 → advance a by 4");
    s("text", { x: ox, y: 200, class: "ts" }, g, "b₃ = 31 ≤ a₃ = 31 → advance b by 4");
  }

  function leapDiagram() {
    const g = $("#leap");
    const lists = { A: [3, 12, 40, 41, 77], B: [5, 15, 40, 60, 77, 90], C: [1, 12, 40, 44, 77, 80] };
    const ys = { A: 56, B: 112, C: 168 };
    const vals = [...new Set(Object.values(lists).flat())].sort((p, q) => p - q);
    const x = (v) => 130 + vals.indexOf(v) * 52;
    [40, 77].forEach((v) => s("rect", { x: x(v) - 11, y: 30, width: 22, height: 156, rx: 6, class: "band" }, g));
    s("text", { x: x(40), y: 204, class: "ts", "text-anchor": "middle" }, g, "result");
    s("text", { x: x(77), y: 204, class: "ts", "text-anchor": "middle" }, g, "result");
    Object.entries(lists).forEach(([name, vals]) => {
      const y = ys[name];
      s("line", { x1: 110, y1: y, x2: 740, y2: y, class: "axis" }, g);
      s("text", { x: 4, y: y + 4, class: "tl" }, g, name === "A" ? "A (rarest)" : name);
      vals.forEach((v) => {
        s("circle", { cx: x(v), cy: y, r: 4.5, class: "dot" }, g);
        s("text", { x: x(v) + 8, y: y + 17, class: "ts" }, g, v);
      });
    });
    const jumps = [["A", 3, "B", 5], ["B", 5, "A", 12], ["A", 12, "B", 15], ["B", 15, "A", 40], ["A", 40, "B", 40], ["B", 40, "C", 40],
      ["A", 41, "B", 60], ["B", 60, "A", 77], ["A", 77, "B", 77], ["B", 77, "C", 77]];
    jumps.forEach(([r1, v1, r2, v2]) => {
      const x1 = x(v1), y1 = ys[r1], x2 = x(v2), y2 = ys[r2];
      const dy = y2 > y1 ? 6 : -6;
      const mx = (x1 + x2) / 2 + (x1 === x2 ? 16 : 0);
      s("path", { d: `M${x1} ${y1 + dy} Q ${mx + 6} ${(y1 + y2) / 2} ${x2} ${y2 - dy}`, class: "ln-acc", "marker-end": "url(#ar3)" }, g);
    });
    s("path", { d: `M${x(40) + 5} ${ys.A - 2} Q ${(x(40) + x(41)) / 2} ${ys.A - 26} ${x(41) - 5} ${ys.A - 4}`, class: "ln" }, g);
    s("text", { x: (x(40) + x(41)) / 2, y: ys.A - 30, class: "ts", "text-anchor": "middle" }, g, "next()");
  }

  // ---------------------------------------------------------------- charts
  const tip = $("#tip");
  function showTip(evt, html) {
    tip.replaceChildren(...html);
    tip.hidden = false;
    const fr = evt.currentTarget.closest(".frame");
    const r = fr.getBoundingClientRect();
    let left = evt.clientX - r.left + fr.scrollLeft + 14;
    let top = evt.clientY - r.top + 14;
    if (left + 230 > r.width) left = Math.max(4, left - 250);
    fr.appendChild(tip);
    tip.style.left = left + "px";
    tip.style.top = top + "px";
  }
  function hideTip() { tip.hidden = true; }
  function tipRows(title, rows) {
    const out = [h("div", { class: "h" }, null, title)];
    rows.forEach(([color, label, value]) => {
      const r = h("div", { class: "row" });
      if (color) { const i = h("i", {}, r); i.style.background = color; }
      h("b", {}, r, value);
      h("span", {}, r, " " + label);
      out.push(r);
    });
    return out;
  }

  function logTicks(min, max) {
    const t = [];
    for (let e = Math.floor(Math.log10(min)); e <= Math.ceil(Math.log10(max)); e++) {
      [1, 2, 5].forEach((m) => { const v = m * 10 ** e; if (v >= min * 0.999 && v <= max * 1.001) t.push(v); });
    }
    return t;
  }
  function linTicks(min, max, n) {
    const span = max - min, step0 = span / n, mag = 10 ** Math.floor(Math.log10(step0));
    const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((st) => span / st <= n) || 10 * mag;
    const t = [];
    for (let v = Math.ceil(min / step) * step; v <= max + 1e-9; v += step) t.push(+v.toFixed(10));
    return t;
  }
  const tickLabel = (v) => (v >= 1000 ? (v / 1000) + "K" : v >= 1 ? String(+v.toFixed(2)) : String(+v.toPrecision(2)));

  function frame(container, height, margin) {
    container.replaceChildren();
    const width = Math.max(300, container.clientWidth);
    const svg = s("svg", { class: "ch", width, height, viewBox: `0 0 ${width} ${height}` }, container);
    return { svg, width, height, iw: width - margin.l - margin.r, ih: height - margin.t - margin.b, m: margin };
  }

  function axes(f, xs, ys, xt, yt, xTitle, yTitle) {
    const g = s("g", {}, f.svg);
    yt.forEach((v) => {
      const y = f.m.t + ys(v);
      s("line", { x1: f.m.l, x2: f.m.l + f.iw, y1: y, y2: y, class: "grid" }, g);
      s("text", { x: f.m.l - 6, y: y + 4, "text-anchor": "end" }, g, tickLabel(v));
    });
    xt.forEach((v) => {
      const x = f.m.l + xs(v);
      s("line", { x1: x, x2: x, y1: f.m.t + f.ih, y2: f.m.t + f.ih + 4, class: "axis" }, g);
      s("text", { x, y: f.m.t + f.ih + 17, "text-anchor": "middle" }, g, tickLabel(v));
    });
    s("line", { x1: f.m.l, x2: f.m.l + f.iw, y1: f.m.t + f.ih, y2: f.m.t + f.ih, class: "axis" }, g);
    s("text", { x: f.m.l + f.iw / 2, y: f.height - 4, "text-anchor": "middle", class: "axis-title" }, g, xTitle);
    s("text", { x: 12, y: f.m.t + f.ih / 2, "text-anchor": "middle", class: "axis-title", transform: `rotate(-90 12 ${f.m.t + f.ih / 2})` }, g, yTitle);
  }

  // Scatter of space vs time with a highlighted Pareto frontier.
  function scatter(container, pts, opt) {
    const f = frame(container, opt.height || 360, { l: 58, r: 20, t: 14, b: 42 });
    const xmin = Math.min(...pts.map((p) => p.x)) / 1.12, xmax = Math.max(...pts.map((p) => p.x)) * 1.12;
    const ylo = Math.min(...pts.map((p) => p.y)) / 1.35, yhi = Math.max(...pts.map((p) => p.y)) * 1.35;
    const xs = (v) => ((Math.log(v) - Math.log(xmin)) / (Math.log(xmax) - Math.log(xmin))) * f.iw;
    const ys = (v) => f.ih - ((Math.log(v) - Math.log(ylo)) / (Math.log(yhi) - Math.log(ylo))) * f.ih;
    axes(f, xs, ys, [4, 5, 6, 8, 10, 12, 16, 20, 32].filter((v) => v >= xmin && v <= xmax), logTicks(ylo, yhi), opt.xTitle, opt.yTitle);
    const sorted = pts.slice().sort((a, b) => a.x - b.x);
    let best = Infinity;
    const front = [];
    sorted.forEach((p) => { if (p.y < best) { best = p.y; front.push(p); p.front = true; } });
    const g = s("g", {}, f.svg);
    s("polyline", { points: front.map((p) => `${f.m.l + xs(p.x)},${f.m.t + ys(p.y)}`).join(" "), fill: "none", stroke: css("--accent"), "stroke-width": 1.5, "stroke-dasharray": "0", opacity: 0.45 }, g);
    const placed = [];
    const overlaps = (b) => placed.some((q) => b.x < q.x + q.w && b.x + b.w > q.x && b.y < q.y + q.h && b.y + b.h > q.y);
    pts.forEach((p) => {
      const cx = f.m.l + xs(p.x), cy = f.m.t + ys(p.y);
      placed.push({ x: cx - 6, y: cy - 6, w: 12, h: 12 });
    });
    pts.forEach((p) => {
      const cx = f.m.l + xs(p.x), cy = f.m.t + ys(p.y);
      const c = s("circle", { cx, cy, r: 5.5, fill: p.front ? css("--accent") : css("--ink-3"), stroke: css("--surface"), "stroke-width": 2, tabindex: 0 }, g);
      const hit = s("circle", { cx, cy, r: 13, fill: "transparent" }, g);
      const show = (e) => showTip(e, tipRows(p.label, [[null, opt.xTitle, fmt(p.x)], [null, "ms/query" + (p.algo ? " · " + p.algo : ""), ms(p.y)]]));
      [c, hit].forEach((el) => { el.addEventListener("pointermove", show); el.addEventListener("pointerleave", hideTip); });
      const w = p.label.length * 6.4 + 4;
      const cands = [[cx + 9, cy + 4], [cx - 9 - w, cy + 4], [cx - w / 2, cy - 11], [cx - w / 2, cy + 19], [cx + 9, cy - 9], [cx + 9, cy + 15], [cx - 9 - w, cy - 9], [cx - 9 - w, cy + 15]];
      for (const [lx, ly] of cands) {
        const box = { x: lx, y: ly - 10, w, h: 13 };
        if (lx < f.m.l || lx + w > f.width - 2 || ly - 10 < 0 || ly > f.m.t + f.ih) continue;
        if (!overlaps(box)) {
          placed.push(box);
          s("text", { x: lx, y: ly, class: p.front ? "lab-strong" : "lab" }, g, p.label);
          break;
        }
      }
    });
  }

  // Horizontal bars, one or more series per category.
  function hbars(container, cats, series, opt) {
    const band = series.length > 1 ? 14 * series.length + 10 : 26;
    const f = frame(container, cats.length * band + 46, { l: opt.labelWidth || 190, r: 64, t: 8, b: 36 });
    const vmax = Math.max(...series.flatMap((sr) => sr.values.filter((v) => v != null))) * 1.05;
    const xs = (v) => (v / vmax) * f.iw;
    const xt = linTicks(0, vmax, 5);
    const g = s("g", {}, f.svg);
    xt.forEach((v) => {
      const x = f.m.l + xs(v);
      s("line", { x1: x, x2: x, y1: f.m.t, y2: f.m.t + f.ih, class: "grid" }, g);
      s("text", { x, y: f.m.t + f.ih + 16, "text-anchor": "middle" }, g, tickLabel(v));
    });
    s("text", { x: f.m.l + f.iw / 2, y: f.height - 4, "text-anchor": "middle", class: "axis-title" }, g, opt.xTitle);
    cats.forEach((cat, i) => {
      const y0 = f.m.t + i * band + 4;
      s("text", { x: f.m.l - 8, y: y0 + band / 2 - 1, "text-anchor": "end", class: "lab" }, g, cat);
      series.forEach((sr, j) => {
        const v = sr.values[i];
        if (v == null) return;
        const bh = series.length > 1 ? 12 : 16;
        const y = y0 + (series.length > 1 ? j * 14 : (band - bh) / 2 - 2);
        const w = Math.max(2, xs(v));
        const r = Math.min(4, w / 2);
        s("path", { d: `M${f.m.l} ${y} H${f.m.l + w - r} Q${f.m.l + w} ${y} ${f.m.l + w} ${y + r} V${y + bh - r} Q${f.m.l + w} ${y + bh} ${f.m.l + w - r} ${y + bh} H${f.m.l} Z`, fill: sr.color }, g);
        s("text", { x: f.m.l + w + 5, y: y + bh - 3, class: "lab" }, g, opt.valueFmt ? opt.valueFmt(v) : fmt(v));
        const hit = s("rect", { x: f.m.l, y: y - 1, width: Math.max(w, 30), height: bh + 2, fill: "transparent" }, g);
        hit.addEventListener("pointermove", (e) => showTip(e, tipRows(cat, series.map((q) => [q.color, q.name, q.values[i] == null ? "—" : (opt.valueFmt ? opt.valueFmt(q.values[i]) : fmt(q.values[i]))]))));
        hit.addEventListener("pointerleave", hideTip);
      });
    });
  }

  // Multi-series line chart; log axes optional; crosshair tooltip.
  function lines(container, xsVals, series, opt) {
    const f = frame(container, opt.height || 340, { l: 58, r: 20, t: 14, b: 42 });
    const all = series.flatMap((sr) => sr.values.filter((v) => v != null));
    const ylo = opt.logY ? Math.min(...all) / 1.3 : 0, yhi = Math.max(...all) * (opt.logY ? 1.3 : 1.08);
    const xlo = Math.min(...xsVals), xhi = Math.max(...xsVals);
    const xs = opt.logX ? (v) => ((Math.log(v) - Math.log(xlo)) / (Math.log(xhi) - Math.log(xlo))) * f.iw : (v) => ((v - xlo) / (xhi - xlo)) * f.iw;
    const ys = opt.logY ? (v) => f.ih - ((Math.log(v) - Math.log(ylo)) / (Math.log(yhi) - Math.log(ylo))) * f.ih : (v) => f.ih - (v / yhi) * f.ih;
    axes(f, xs, ys, opt.xTicks || xsVals, opt.logY ? logTicks(ylo, yhi) : linTicks(0, yhi, 5), opt.xTitle, opt.yTitle);
    const g = s("g", {}, f.svg);
    series.forEach((sr) => {
      const pts = xsVals.map((x, i) => [x, sr.values[i]]).filter((p) => p[1] != null);
      s("polyline", { points: pts.map(([x, y]) => `${f.m.l + xs(x)},${f.m.t + ys(y)}`).join(" "), fill: "none", stroke: sr.color, "stroke-width": 2, "stroke-linejoin": "round", "stroke-linecap": "round" }, g);
      pts.forEach(([x, y]) => s("circle", { cx: f.m.l + xs(x), cy: f.m.t + ys(y), r: 3.5, fill: sr.color, stroke: css("--surface"), "stroke-width": 1.5 }, g));
    });
    const cross = s("line", { y1: f.m.t, y2: f.m.t + f.ih, stroke: css("--ink-3"), "stroke-width": 1, opacity: 0 }, g);
    const hit = s("rect", { x: f.m.l, y: f.m.t, width: f.iw, height: f.ih, fill: "transparent" }, g);
    hit.addEventListener("pointermove", (e) => {
      const r = f.svg.getBoundingClientRect();
      const px = e.clientX - r.left - f.m.l;
      let bi = 0;
      xsVals.forEach((x, i) => { if (Math.abs(xs(x) - px) < Math.abs(xs(xsVals[bi]) - px)) bi = i; });
      const cx = f.m.l + xs(xsVals[bi]);
      cross.setAttribute("x1", cx); cross.setAttribute("x2", cx); cross.setAttribute("opacity", 0.5);
      const rows = series.map((sr) => [sr.color, sr.name, sr.values[bi] == null ? "—" : (opt.valueFmt || fmt)(sr.values[bi])])
        .sort((a, b) => parseFloat(a[2]) - parseFloat(b[2]));
      showTip(e, tipRows((opt.xLabel || "") + xsVals[bi], rows));
    });
    hit.addEventListener("pointerleave", () => { cross.setAttribute("opacity", 0); hideTip(); });
  }

  function legend(container, series, square) {
    const l = h("div", { class: "legend" });
    series.forEach((sr) => {
      const sp = h("span", {}, l);
      const i = h("i", square ? { class: "sq" } : {}, sp);
      i.style.background = sr.color;
      sp.appendChild(document.createTextNode(sr.name));
    });
    container.parentNode.insertBefore(l, container);
  }

  // ---------------------------------------------------------------- data helpers
  const isCount = (m) => m.algorithm.includes("count only");
  function best(c, op) {
    return c.measurements.filter((m) => m.op === op && !isCount(m)).sort((a, b) => a.ms_per_query - b.ms_per_query)[0];
  }
  function m(c, op, algo) {
    const x = c.measurements.find((q) => q.op === op && q.algorithm === algo);
    return x ? x.ms_per_query : null;
  }
  const short = (name) => name.replace("Hybrid (bitmap + BP128)", "Hybrid").replace("BP128 (bitpacking crate)", "BP128 (crate)").replace("Roaring (roaring crate)", "Roaring (crate)");

  const shortAlgo = (a) => a.replace(" (NextGEQ)", "").replace(" (4096)", "").replace("adaptive (SIMD / gallop @64x)", "adaptive").replace("slices: ", "");
  function summaryTable(container, run) {
    const wrap = h("div", { class: "table-wrap" }, container);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    ["codec", "bits/posting", "query lists", "decode ns/posting", "best AND ms", "", "best OR ms", ""].forEach((x, i) =>
      h("th", { class: i && i !== 5 && i !== 7 ? "n" : "" }, hr, x));
    const tb = h("tbody", {}, t);
    const bAnd = Math.min(...run.codecs.map((c) => best(c, "AND").ms_per_query));
    const bOr = Math.min(...run.codecs.map((c) => best(c, "OR").ms_per_query));
    const bSpace = Math.min(...run.codecs.map((c) => c.query_bits_per_posting));
    run.codecs.forEach((c) => {
      const tr = h("tr", {}, tb);
      const a = best(c, "AND"), o = best(c, "OR");
      h("td", {}, tr, c.codec);
      h("td", { class: "n" }, tr, c.bits_per_posting.toFixed(2));
      h("td", { class: "n" + (c.query_bits_per_posting === bSpace ? " best" : "") }, tr, c.query_bits_per_posting.toFixed(2));
      h("td", { class: "n" }, tr, c.query_decode_ns_per_posting.toFixed(2));
      h("td", { class: "n" + (a.ms_per_query === bAnd ? " best" : "") }, tr, ms(a.ms_per_query));
      h("td", { class: "algo" }, tr, shortAlgo(a.algorithm));
      h("td", { class: "n" + (o.ms_per_query === bOr ? " best" : "") }, tr, ms(o.ms_per_query));
      h("td", { class: "algo" }, tr, shortAlgo(o.algorithm));
    });
  }

  function algoTable(container, run, op, algos) {
    const wrap = h("div", { class: "table-wrap" }, container);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    h("th", {}, hr, "codec");
    algos.forEach((a) => h("th", { class: "n" }, hr, a));
    const tb = h("tbody", {}, t);
    run.codecs.forEach((c) => {
      const vals = algos.map((a) => m(c, op, a));
      if (vals.every((v) => v == null)) return;
      const mn = Math.min(...vals.filter((v, i) => v != null && !algos[i].includes("count only")));
      const tr = h("tr", {}, tb);
      h("td", {}, tr, c.codec);
      vals.forEach((v) => h("td", { class: "n" + (v === mn ? " best" : "") }, tr, v == null ? "—" : ms(v)));
    });
  }

  function allTable(container, run) {
    const d = h("details", {}, container);
    h("summary", {}, d, "All measurements (" + run.codecs.reduce((n, c) => n + c.measurements.length, 0) + " rows)");
    const wrap = h("div", { class: "table-wrap" }, d);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    ["codec", "op", "algorithm", "ms/query"].forEach((x, i) => h("th", { class: i === 3 ? "n" : "" }, hr, x));
    const tb = h("tbody", {}, t);
    run.codecs.forEach((c) => c.measurements.forEach((q) => {
      const tr = h("tr", {}, tb);
      h("td", {}, tr, c.codec); h("td", {}, tr, q.op); h("td", {}, tr, q.algorithm); h("td", { class: "n" }, tr, ms(q.ms_per_query));
    }));
  }

  // ---------------------------------------------------------------- page assembly
  const renderers = [];
  function chart(id, fn) {
    const el = document.getElementById(id);
    if (!el) return;
    renderers.push(() => fn(el));
  }

  function setupTable() {
    const runs = [["clustered", "Synthetic, clustered"], ["uniform", "Synthetic, uniform"], ["wikitext", "WikiText-103"]].filter(([k]) => DATA[k]);
    const wrap = h("div", { class: "table-wrap" }, $("#setup-table"));
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    ["collection", "documents", "lists", "postings", "queries", "terms/query", "AND results", "OR results"].forEach((x, i) => h("th", { class: i ? "n" : "" }, hr, x));
    const tb = h("tbody", {}, t);
    runs.forEach(([k, name]) => {
      const r = DATA[k];
      const tr = h("tr", {}, tb);
      h("td", {}, tr, name);
      [r.documents, r.lists, r.postings, r.queries].forEach((v) => h("td", { class: "n" }, tr, v.toLocaleString("en-US")));
      h("td", { class: "n" }, tr, r.mean_terms.toFixed(2));
      h("td", { class: "n" }, tr, Math.round(r.mean_and_results).toLocaleString("en-US"));
      h("td", { class: "n" }, tr, Math.round(r.mean_or_results).toLocaleString("en-US"));
    });
    const p = h("p", { class: "meta" }, $("#setup-table"));
    p.textContent = "CPU: " + DATA.clustered.cpu + " · SIMD: " + DATA.clustered.simd.join(", ") + " · single thread · Rust " + (DATA.rustc || "stable") + " · best of " + DATA.clustered.reps + " passes";
  }

  function codecCards() {
    const rows = [
      ["Uncompressed", "baseline", "sorted u32 array", "galloping", "—"],
      ["VByte", "block", "varint d-gaps, 128 per block + skip table", "skip table, decode block", "—"],
      ["BP128", "block", "one bit width per 128 gaps (SIMD-BP128 layout)", "skip table, decode block", "—"],
      ["BP128 (bitpacking crate)", "baseline", "Tantivy's BitPacker4x in the same blocks", "skip table, decode block", "—"],
      ["OptPFor", "block", "per-block optimal width + patched exceptions", "skip table, decode block", "—"],
      ["Interpolative", "block", "binary interpolative coding per block", "skip table, decode block", "—"],
      ["Elias-Fano", "succinct", "low bits + unary high bits", "select₀ on high bits", "—"],
      ["PEF", "succinct", "DP partitions: run, bitvector or Elias-Fano", "gallop partitions, then local", "—"],
      ["Roaring", "bitmap", "2¹⁶ chunks: array, bitmap or run containers", "gallop keys, then container", "SvS over containers, lazy k-way OR"],
      ["Roaring (roaring crate)", "baseline", "the roaring crate, run-optimised", "iterator advance_to", "MultiOps"],
      ["Hybrid (bitmap + BP128)", "bitmap", "bitmap if u/n ≤ 8, else BP128", "word scan or block", "leapfrog sparse lists + bit probes"],
      ["Trie", "succinct", "2 bits per trie node, rank navigation", "descend along target bits", "synchronised traversal"],
      ["Run-Trie", "succinct", "trie with full subtrees cut", "descend along target bits", "synchronised traversal"],
    ];
    const wrap = h("div", { class: "table-wrap" }, $("#codec-cards"));
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    ["codec", "family", "layout", "NextGEQ", "native AND / OR"].forEach((x) => h("th", {}, hr, x));
    const tb = h("tbody", {}, t);
    rows.forEach((r) => { const tr = h("tr", {}, tb); r.forEach((c, i) => h("td", i === 0 ? { class: "strong" } : {}, tr, c)); });
  }

  function runSection(key) {
    const run = DATA[key];
    if (!run) return;
    chart(key + "-and", (el) => scatter(el, run.codecs.map((c) => ({ label: short(c.codec), x: c.query_bits_per_posting, y: best(c, "AND").ms_per_query, algo: best(c, "AND").algorithm })), { xTitle: "bits per posting, query lists (log)", yTitle: "AND ms/query (log)" }));
    chart(key + "-or", (el) => scatter(el, run.codecs.map((c) => ({ label: short(c.codec), x: c.query_bits_per_posting, y: best(c, "OR").ms_per_query, algo: best(c, "OR").algorithm })), { xTitle: "bits per posting, query lists (log)", yTitle: "OR ms/query (log)" }));
    const tbl = document.getElementById(key + "-table");
    if (tbl) { summaryTable(tbl, run); allTable(tbl, run); }
  }

  function render() { renderers.forEach((r) => r()); }

  function init() {
    if (!DATA) return;
    blockDiagram(); efDiagram(); pefDiagram(); roaringDiagram(); trieDiagram(); simdDiagram(); leapDiagram();
    codecCards();
    setupTable();
    ["clustered", "uniform", "wikitext"].forEach(runSection);

    const ANDALGOS = ["leapfrog (NextGEQ)", "round-robin (LFTJ)", "decode + SvS", "native", "native, count only"];
    const ORALGOS = ["heap", "min-scan", "bitset window (4096)", "native", "native, count only"];
    const at = $("#algo-and"); if (at) algoTable(at, DATA.clustered, "AND", ANDALGOS);
    const ot = $("#algo-or"); if (ot) algoTable(ot, DATA.clustered, "OR", ORALGOS);

    if (DATA.uniform) {
      chart("space-compare", (el) => {
        const names = DATA.clustered.codecs.map((c) => c.codec).filter((n) => n !== "Uncompressed");
        const get = (run) => names.map((n) => { const c = run.codecs.find((q) => q.codec === n); return c ? c.query_bits_per_posting : null; });
        const series = [{ name: "clustered", color: css("--s1"), values: get(DATA.clustered) }, { name: "uniform", color: css("--s2"), values: get(DATA.uniform) }];
        if (!el.previousElementSibling || !el.previousElementSibling.classList.contains("legend")) legend(el, series, true);
        hbars(el, names.map(short), series, { xTitle: "bits per posting (query lists)", labelWidth: 120 });
      });
      chart("time-compare", (el) => {
        const names = DATA.clustered.codecs.map((c) => c.codec);
        const get = (run) => names.map((n) => { const c = run.codecs.find((q) => q.codec === n); return c ? best(c, "AND").ms_per_query : null; });
        const series = [{ name: "clustered", color: css("--s1"), values: get(DATA.clustered) }, { name: "uniform", color: css("--s2"), values: get(DATA.uniform) }];
        if (!el.previousElementSibling || !el.previousElementSibling.classList.contains("legend")) legend(el, series, true);
        hbars(el, names.map(short), series, { xTitle: "best AND ms/query", labelWidth: 120, valueFmt: ms });
      });
    }

    if (DATA.kernels) {
      chart("kernels", (el) => {
        const rows = DATA.kernels.rows;
        const ratios = [...new Set(rows.map((r) => r.ratio))];
        const pick = ["merge", "merge (branchless)", "galloping", "binary search", "Baeza-Yates", "SIMD AVX-512 16x16", "adaptive (SIMD / gallop @64x)"];
        const cols = ["--s1", "--s2", "--s3", "--s4", "--s5", "--s6", "--s7"];
        const series = pick.filter((k) => rows.some((r) => r.kernel === k)).map((k, i) => ({
          name: k.replace(" (SIMD / gallop @64x)", " (SIMD ≤64×, else gallop)"), color: css(cols[i]),
          values: ratios.map((x) => { const r = rows.find((q) => q.kernel === k && q.ratio === x); return r ? r.ns_per_small_element : null; }),
        }));
        if (!el.previousElementSibling || !el.previousElementSibling.classList.contains("legend")) legend(el, series);
        lines(el, ratios, series, { logX: true, logY: true, xTitle: "size ratio |large| / |small| (log)", yTitle: "ns per element of the small list (log)", xLabel: "ratio 1:", valueFmt: (v) => fmt(v) + " ns" });
      });
    }

    if (DATA.terms) {
      chart("terms", (el) => {
        const ks = Object.keys(DATA.terms).map(Number).sort((a, b) => a - b);
        const pick = ["Uncompressed", "BP128", "PEF", "Hybrid (bitmap + BP128)", "Roaring", "Trie"];
        const cols = ["--s1", "--s2", "--s3", "--s4", "--s5", "--s6"];
        const series = pick.map((n, i) => ({
          name: short(n), color: css(cols[i]),
          values: ks.map((k) => { const c = DATA.terms[k].codecs.find((q) => q.codec === n); return c ? best(c, "AND").ms_per_query : null; }),
        }));
        if (!el.previousElementSibling || !el.previousElementSibling.classList.contains("legend")) legend(el, series);
        lines(el, ks, series, { logY: true, xTitle: "terms per query", yTitle: "best AND ms/query (log)", xLabel: "k = ", valueFmt: ms });
      });
    }

    const f = $("#footer");
    if (f) f.textContent = "Generated from results/*.json by scripts/build_report.py · " + (DATA.generated || "");
    const meta = $("#meta-line");
    if (meta && DATA.clustered) meta.textContent = DATA.clustered.cpu + " · single thread · Rust, -C target-cpu=native";

    render();
    let t;
    window.addEventListener("resize", () => { clearTimeout(t); t = setTimeout(render, 120); });
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    if (mq.addEventListener) mq.addEventListener("change", render);
    new MutationObserver(render).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init); else init();
})();
