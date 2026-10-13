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
    pts.slice().sort((a, b) => (b.strong ? 1 : 0) - (a.strong ? 1 : 0)).forEach((p) => {
      const cx = f.m.l + xs(p.x), cy = f.m.t + ys(p.y);
      const c = s("circle", { cx, cy, r: p.color ? 6.5 : 5.5, fill: p.color || (p.front ? css("--accent") : css("--ink-3")), stroke: css("--surface"), "stroke-width": 2, tabindex: 0 }, g);
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
          s("text", { x: lx, y: ly, class: p.front || p.strong ? "lab-strong" : "lab" }, g, p.label);
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


  window.PB = { s, h, fmt, ms, css, scatter, hbars, lines, legend, showTip, hideTip, tipRows };
})();
