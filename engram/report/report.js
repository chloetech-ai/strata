// Charts and tables of the Engram report, drawn from DATA (scripts/build_report.py).
(function () {
  "use strict";
  const { s, h, fmt, ms, css, showTip, hideTip, tipRows } = window.PB;
  const $ = (q) => document.querySelector(q);
  const D = DATA;
  if (!D) return;
  const num = (v, d) => (v == null || !isFinite(v) ? "—" : v.toFixed(d));
  const pct = (v) => (v == null || !isFinite(v) ? "—" : (100 * v).toFixed(0) + "%");

  // ------------------------------------------------------------ helpers
  function frame(container, height, m) {
    container.replaceChildren();
    const width = Math.max(300, container.clientWidth);
    const svg = s("svg", { class: "ch", width, height, viewBox: `0 0 ${width} ${height}` }, container);
    return { svg, width, height, iw: width - m.l - m.r, ih: height - m.t - m.b, m };
  }
  function logTicks(min, max) {
    const t = [];
    for (let e = Math.floor(Math.log10(min)); e <= Math.ceil(Math.log10(max)); e++) {
      [1, 2, 5].forEach((k) => { const v = k * 10 ** e; if (v >= min * 0.999 && v <= max * 1.001) t.push(v); });
    }
    return t.length > 7 ? t.filter((v) => /^1/.test(v.toPrecision(1))) : t;
  }
  const tick = (v) => (v >= 1000 ? +(v / 1000).toPrecision(3) + "K" : v >= 1 ? String(+v.toPrecision(3)) : String(+v.toPrecision(2)));

  function tabs(container, items, onPick, initial) {
    const bar = h("div", { class: "tabs", role: "group" });
    container.parentNode.insertBefore(bar, container);
    const btns = items.map(([key, label]) => {
      const b = h("button", { type: "button", "aria-pressed": "false" }, bar, label);
      b.addEventListener("click", () => pick(key));
      return [key, b];
    });
    function pick(key) {
      btns.forEach(([k, b]) => b.setAttribute("aria-pressed", String(k === key)));
      onPick(key);
    }
    pick(initial || items[0][0]);
    return pick;
  }

  function legend(container, series) {
    const l = h("div", { class: "legend" });
    series.forEach((sr) => {
      const sp = h("span", {}, l);
      const i = h("i", { class: "sq" }, sp);
      i.style.background = sr.color;
      sp.appendChild(document.createTextNode(sr.name));
    });
    container.parentNode.insertBefore(l, container);
  }

  // Grouped horizontal bars on a log axis (latencies span four decades).
  function logBars(container, cats, series, opt) {
    const band = 14 * series.length + 12;
    const f = frame(container, cats.length * band + 46, { l: opt.labelWidth || 150, r: 58, t: 6, b: 36 });
    const vals = series.flatMap((sr) => sr.values.filter((v) => v != null && v > 0));
    if (!vals.length) return;
    const lo = Math.min(...vals) / 1.6, hi = Math.max(...vals) * 1.25;
    const xs = (v) => ((Math.log(v) - Math.log(lo)) / (Math.log(hi) - Math.log(lo))) * f.iw;
    const g = s("g", {}, f.svg);
    logTicks(lo, hi).forEach((v) => {
      const x = f.m.l + xs(v);
      s("line", { x1: x, x2: x, y1: f.m.t, y2: f.m.t + f.ih, class: "grid" }, g);
      s("text", { x, y: f.m.t + f.ih + 16, "text-anchor": "middle" }, g, tick(v));
    });
    s("text", { x: f.m.l + f.iw / 2, y: f.height - 4, "text-anchor": "middle", class: "axis-title" }, g, opt.xTitle);
    cats.forEach((cat, i) => {
      let y0 = f.m.t + i * band + 5;
      if (narrow) {
        s("text", { x: f.m.l, y: y0 + 9, class: "lab" }, g, cat);
        y0 += 16;
      } else {
        s("text", { x: f.m.l - 8, y: y0 + band / 2 - 2, "text-anchor": "end", class: "lab" }, g, cat);
      }
      series.forEach((sr, j) => {
        const v = sr.values[i];
        const y = y0 + j * 14;
        if (v == null) {
          s("text", { x: f.m.l + 4, y: y + 10, class: "lab" }, g, sr.missing || "n/a");
          return;
        }
        const w = Math.max(2, xs(v));
        s("rect", { x: f.m.l, y, width: w, height: 12, rx: 2.5, fill: sr.color }, g);
        s("text", { x: f.m.l + w + 5, y: y + 10, class: "lab" }, g, opt.valueFmt(v));
        const hit = s("rect", { x: f.m.l, y: y - 1, width: Math.max(w, 30), height: 14, fill: "transparent" }, g);
        hit.addEventListener("pointermove", (e) => showTip(e, tipRows(cat, series.map((q) => [q.color, q.name, q.values[i] == null ? q.missing || "n/a" : opt.valueFmt(q.values[i])]))));
        hit.addEventListener("pointerleave", hideTip);
      });
    });
  }

  // Curves with their own x values per series (e.g. recall vs latency).
  function xy(container, series, opt) {
    const f = frame(container, opt.height || 330, { l: 58, r: 20, t: 14, b: 42 });
    const pts = series.flatMap((sr) => sr.points);
    if (!pts.length) return;
    const xv = pts.map((p) => p[0]), yv = pts.map((p) => p[1]);
    const xlo = opt.logX ? Math.min(...xv) / 1.4 : opt.xMin ?? Math.min(...xv), xhi = opt.logX ? Math.max(...xv) * 1.4 : opt.xMax ?? Math.max(...xv);
    const ylo = opt.logY ? Math.min(...yv) / 1.4 : opt.yMin ?? 0, yhi = opt.logY ? Math.max(...yv) * 1.4 : opt.yMax ?? Math.max(...yv) * 1.06;
    const sc = (v, lo, hi, log, len) => (log ? (Math.log(v) - Math.log(lo)) / (Math.log(hi) - Math.log(lo)) : (v - lo) / (hi - lo)) * len;
    const xs = (v) => sc(v, xlo, xhi, opt.logX, f.iw);
    const ys = (v) => f.ih - sc(v, ylo, yhi, opt.logY, f.ih);
    const g = s("g", {}, f.svg);
    const lin = (lo, hi, n) => {
      const step0 = (hi - lo) / n, mag = 10 ** Math.floor(Math.log10(step0));
      const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((st) => (hi - lo) / st <= n);
      const t = [];
      for (let v = Math.ceil(lo / step) * step; v <= hi + 1e-9; v += step) t.push(+v.toFixed(10));
      return t;
    };
    const yt = opt.logY ? logTicks(ylo, yhi) : lin(ylo, yhi, 5);
    const xt = opt.xTicks || (opt.logX ? logTicks(xlo, xhi) : lin(xlo, xhi, 6));
    yt.forEach((v) => {
      const y = f.m.t + ys(v);
      s("line", { x1: f.m.l, x2: f.m.l + f.iw, y1: y, y2: y, class: "grid" }, g);
      s("text", { x: f.m.l - 6, y: y + 4, "text-anchor": "end" }, g, opt.yFmt ? opt.yFmt(v) : tick(v));
    });
    xt.forEach((v) => {
      const x = f.m.l + xs(v);
      s("line", { x1: x, x2: x, y1: f.m.t + f.ih, y2: f.m.t + f.ih + 4, class: "axis" }, g);
      s("text", { x, y: f.m.t + f.ih + 17, "text-anchor": "middle" }, g, opt.xFmt ? opt.xFmt(v) : tick(v));
    });
    s("line", { x1: f.m.l, x2: f.m.l + f.iw, y1: f.m.t + f.ih, y2: f.m.t + f.ih, class: "axis" }, g);
    s("text", { x: f.m.l + f.iw / 2, y: f.height - 4, "text-anchor": "middle", class: "axis-title" }, g, opt.xTitle);
    s("text", { x: 12, y: f.m.t + f.ih / 2, "text-anchor": "middle", class: "axis-title", transform: `rotate(-90 12 ${f.m.t + f.ih / 2})` }, g, opt.yTitle);
    series.forEach((sr) => {
      const P = sr.points.slice().sort((a, b) => a[0] - b[0]);
      if (P.length > 1) s("polyline", { points: P.map(([x, y]) => `${f.m.l + xs(x)},${f.m.t + ys(y)}`).join(" "), fill: "none", stroke: sr.color, "stroke-width": sr.width || 2, "stroke-dasharray": sr.dash || "0", "stroke-linejoin": "round" }, g);
      P.forEach((p) => {
        const c = s("circle", { cx: f.m.l + xs(p[0]), cy: f.m.t + ys(p[1]), r: sr.r || 3.5, fill: sr.color, stroke: css("--surface"), "stroke-width": 1.5 }, g);
        const hit = s("circle", { cx: f.m.l + xs(p[0]), cy: f.m.t + ys(p[1]), r: 10, fill: "transparent" }, g);
        const rows = [[sr.color, opt.xTitle, opt.xFmt ? opt.xFmt(p[0]) : fmt(p[0])], [null, opt.yTitle, opt.yFmt ? opt.yFmt(p[1]) : fmt(p[1])]];
        if (p[2]) rows.push([null, "setting", p[2]]);
        [c, hit].forEach((el) => {
          el.addEventListener("pointermove", (e) => showTip(e, tipRows(sr.name, rows)));
          el.addEventListener("pointerleave", hideTip);
        });
      });
    });
  }

  function table(container, head, rows, numCols) {
    const wrap = h("div", { class: "table-wrap compact wrap-head" }, container);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    head.forEach((x, i) => h("th", { class: numCols.includes(i) ? "n" : "" }, hr, x));
    const tb = h("tbody", {}, t);
    rows.forEach((r) => {
      const tr = h("tr", {}, tb);
      r.forEach((x, i) => {
        const cls = [numCols.includes(i) ? "n" : "", x && x.cls ? x.cls : ""].join(" ").trim();
        h("td", cls ? { class: cls } : {}, tr, x && x.v !== undefined ? x.v : x);
      });
    });
  }


  // Horizontal grouped bars on a linear 0..1 axis.
  function bars(container, cats, series, opt) {
    const narrow = container.clientWidth < 560;
    const band = 14 * series.length + 12 + (narrow ? 16 : 0);
    const f = frame(container, cats.length * band + 46, { l: narrow ? 6 : opt.labelWidth || 170, r: 52, t: 6, b: 36 });
    const xs = (v) => (v / (opt.max || 1)) * f.iw;
    const g = s("g", {}, f.svg);
    (narrow ? [0, 0.5, 1] : opt.ticks || [0, 0.25, 0.5, 0.75, 1]).forEach((v) => {
      const x = f.m.l + xs(v);
      s("line", { x1: x, x2: x, y1: f.m.t, y2: f.m.t + f.ih, class: "grid" }, g);
      s("text", { x, y: f.m.t + f.ih + 16, "text-anchor": "middle" }, g, opt.tickFmt ? opt.tickFmt(v) : v.toFixed(2));
    });
    s("text", { x: f.m.l + f.iw / 2, y: f.height - 4, "text-anchor": "middle", class: "axis-title" }, g, opt.xTitle);
    cats.forEach((cat, i) => {
      let y0 = f.m.t + i * band + 5;
      if (narrow) {
        s("text", { x: f.m.l, y: y0 + 9, class: "lab" }, g, cat);
        y0 += 16;
      } else {
        s("text", { x: f.m.l - 8, y: y0 + band / 2 - 2, "text-anchor": "end", class: "lab" }, g, cat);
      }
      series.forEach((sr, j) => {
        const v = sr.values[i];
        const y = y0 + j * 14;
        if (v == null) {
          s("text", { x: f.m.l + 4, y: y + 10, class: "lab" }, g, sr.missing || "n/a");
          return;
        }
        const w = Math.max(1.5, xs(v));
        s("rect", { x: f.m.l, y, width: w, height: 12, rx: 2.5, fill: sr.color }, g);
        s("text", { x: f.m.l + w + 5, y: y + 10, class: "lab" }, g, opt.valueFmt(v));
        const hit = s("rect", { x: f.m.l, y: y - 1, width: Math.max(w, 30), height: 14, fill: "transparent" }, g);
        hit.addEventListener("pointermove", (e) => showTip(e, tipRows(cat, series.map((q) => [q.color, q.name, q.values[i] == null ? "n/a" : opt.valueFmt(q.values[i])]))));
        hit.addEventListener("pointerleave", hideTip);
      });
    });
  }

  // One stacked horizontal bar per category; parts sum to 1.
  function stacked(container, cats, parts, opt) {
    const narrow = container.clientWidth < 560;
    const band = narrow ? 50 : 34;
    const f = frame(container, cats.length * band + 50, { l: narrow ? 6 : opt.labelWidth || 230, r: 16, t: 6, b: 38 });
    const g = s("g", {}, f.svg);
    [0, 0.25, 0.5, 0.75, 1].forEach((v) => {
      const x = f.m.l + v * f.iw;
      s("line", { x1: x, x2: x, y1: f.m.t, y2: f.m.t + f.ih, class: "grid" }, g);
      s("text", { x, y: f.m.t + f.ih + 16, "text-anchor": "middle" }, g, (100 * v).toFixed(0) + "%");
    });
    s("text", { x: f.m.l + f.iw / 2, y: f.height - 4, "text-anchor": "middle", class: "axis-title" }, g, opt.xTitle);
    cats.forEach((cat, i) => {
      let y = f.m.t + i * band + 8;
      const label = cat.label.split("\n");
      if (narrow) {
        s("text", { x: f.m.l, y: y + 8, class: "lab" }, g, label.join(", "));
        y += 16;
      } else {
        label.forEach((t, k) => s("text", { x: f.m.l - 8, y: y + 9 + k * 13 - (label.length - 1) * 6, "text-anchor": "end", class: "lab" }, g, t));
      }
      let x = f.m.l;
      parts.forEach((p, j) => {
        const v = cat.values[j] || 0;
        const w = v * f.iw;
        if (w <= 0) return;
        const r = s("rect", { x, y, width: Math.max(w - 1, 0.5), height: 20, fill: p.color }, g);
        if (w > 26) s("text", { x: x + w / 2, y: y + 14, "text-anchor": "middle", class: "lab", style: "fill: var(--surface)" }, g, (100 * v).toFixed(0) + "%");
        r.addEventListener("pointermove", (e) => showTip(e, tipRows(cat.label.replace("\n", " "), parts.map((q, k) => [q.color, q.name, (100 * (cat.values[k] || 0)).toFixed(0) + "%"]).concat([[null, "probes", String(cat.n)]]))));
        r.addEventListener("pointerleave", hideTip);
        x += w;
      });
    });
  }

  // ------------------------------------------------------------ sections
  function meta() {
    const m = $("#meta-line");
    if (m) m.textContent = "Intel Xeon (Emerald Rapids), 4 cores · Rust · Strata storage and search · static embedder potion-retrieval-32M (512-d), BGE-small for comparison · Claude Code 2.1 hooks and MCP";
    const f = $("#footer");
    if (f) f.textContent = `Generated from engram/results/ by scripts/build_report.py · ${D.generated}`;
  }

  const R = D.retrieval || {};
  const METHODS = [
    ["bm25", "BM25"],
    ["dense", "vectors"],
    ["hybrid_0.5_priors", "hybrid + priors (default)"],
  ];

  function retrieval() {
    const el = $("#retrieval-chart");
    if (!el) return;
    const metric = [["recall_any@5", "evidence in top 5"], ["recall_any@10", "evidence in top 10"], ["pack4k", "evidence in the 4,000-character injection"]];
    const colors = ["--s1", "--s3", "--s2"].map(css);
    legend(el, metric.map(([, n], i) => ({ name: n, color: colors[i] })));
    let which = "lme";
    const draw = () => {
      const rows = [];
      const add = (label, d, m) => d && rows.push([label, d.all[m]]);
      if (which === "lme") {
        add("BM25", R.lme_static, "bm25");
        add("vectors, static", R.lme_static, "dense");
        add("hybrid + priors, static", R.lme_static, "hybrid_0.5_priors");
        add("vectors, static (subset)", R.lme_static_sub, "dense");
        add("vectors, BGE (subset)", R.lme_bge_sub, "dense");
        add("hybrid + priors, static (subset)", R.lme_static_sub, "hybrid_0.5_priors");
        add("hybrid + priors, BGE (subset)", R.lme_bge_sub, "hybrid_0.5_priors");
      } else {
        add("BM25", R.locomo_static, "bm25");
        add("vectors, static", R.locomo_static, "dense");
        add("hybrid + priors, static", R.locomo_static, "hybrid_0.5_priors");
        add("vectors, BGE", R.locomo_bge, "dense");
        add("hybrid + priors, BGE", R.locomo_bge, "hybrid_0.5_priors");
      }
      bars(el, rows.map((r) => r[0]), metric.map(([k, n], i) => ({ name: n, color: colors[i], values: rows.map((r) => r[1] && r[1][k]) })), { xTitle: "share of answerable questions", valueFmt: (v) => v.toFixed(3), labelWidth: 200 });
    };
    tabs(el, [["lme", "LongMemEval-S (470 questions)"], ["locomo", "LoCoMo (1,536 questions)"]], (k) => { which = k; draw(); }, "lme");
    window.addEventListener("resize", draw);
  }

  function typeTable() {
    const el = $("#type-table");
    const d = R.lme_static;
    if (!el || !d) return;
    const rows = Object.entries(d.by_type).map(([t, v]) => [t, v.n, num(v.bm25["recall_any@5"], 3), num(v.dense["recall_any@5"], 3), { v: num(v["hybrid_0.5_priors"]["recall_any@5"], 3), cls: "best" }, num(v["hybrid_0.5_priors"]["session_all@5"], 3), num(v["hybrid_0.5_priors"].pack4k, 3)]);
    table(el, ["question type", "n", "BM25 R@5", "vectors R@5", "hybrid R@5", "hybrid: all evidence sessions in top 5", "evidence in injection"], rows, [1, 2, 3, 4, 5, 6]);
  }

  function ctxTable() {
    const el = $("#ctx-table");
    if (!el) return;
    const rows = [];
    const m = (d, k) => (d ? d.all["hybrid_0.5_priors"][k] : null);
    [["LongMemEval-S", R.lme_static_none, R.lme_static, R.lme_static_request], ["LoCoMo", R.locomo_static_none, R.locomo_static, R.locomo_static]].forEach(([name, a, b, c]) => {
      if (!a || !b) return;
      const label = { "recall_any@5": "evidence in top 5", "recall_any@10": "evidence in top 10", pack4k: "evidence in injection" };
      Object.keys(label).forEach((k, i) => {
        rows.push([i === 0 ? name : "", label[k], num(m(a, k), 3), num(m(b, k), 3), num(m(c, k), 3)]);
      });
    });
    table(el, ["benchmark", "metric", "text only", "with the exchange partner", "responses with their request (Engram)"], rows, [2, 3, 4]);
  }

  function probes() {
    const el = $("#probes-chart");
    const P = D.probes || {};
    if (!el) return;
    const parts = [
      { name: "still in context (summary or live window)", color: css("--s1") },
      { name: "recovered by automatic injection", color: css("--s3") },
      { name: "recovered by one memory_search", color: css("--s4") },
      { name: "not recovered", color: css("--s8") },
    ];
    legend(el, parts);
    const cats = [];
    const sets = [["lost", "lost details (18)"], ["vague", "lost details, vague wording (12)"], ["recurring", "recurring facts (40)"]];
    sets.forEach(([key, name]) => {
      const rows = P[key];
      if (!rows) return;
      [["4633:5392", false, "after compaction 2"], ["4633:5392", true, "new session"]].forEach(([pt, fresh, where]) => {
        const sel = rows.filter((r) => r.point === pt && r.fresh === fresh && r.stored);
        const n = sel.length || 1;
        const ctx = sel.filter((r) => r.in_context).length / n;
        const auto = sel.filter((r) => !r.in_context && r.auto_hit).length / n;
        const pull = sel.filter((r) => !r.in_context && !r.auto_hit && r.pull8_hit).length / n;
        cats.push({ label: `${name}\n${where}`, values: [ctx, auto, pull, 1 - ctx - auto - pull], n: sel.length });
      });
    });
    const draw = () => stacked(el, cats, parts, { xTitle: "share of probes whose answer is stored", labelWidth: 240 });
    draw();
    window.addEventListener("resize", draw);
  }

  function probeTable() {
    const el = $("#probe-table");
    const P = D.probes || {}, B = D.probes_bge || {};
    if (!el) return;
    const rows = [];
    [["lost", "lost details"], ["vague", "lost details, vague"], ["recurring", "recurring facts"]].forEach(([k, name]) => {
      [[false, "after compaction 2"], [true, "new session"]].forEach(([fresh, where]) => {
        const f = (set) => (set || []).filter((r) => r.point === "4633:5392" && r.fresh === fresh && r.stored);
        const a = f(P[k]), b = f(B[k]);
        if (!a.length) return;
        const share = (sel, fn) => (sel.length ? sel.filter(fn).length / sel.length : null);
        const bb = (fn) => (b.length ? pct(share(b, fn)) : "—");
        rows.push([name, where, a.length, pct(share(a, (r) => r.in_context)), pct(share(a, (r) => r.in_summary)),
          pct(share(a, (r) => r.in_context || r.auto_hit)), bb((r) => r.in_context || r.auto_hit),
          pct(share(a, (r) => r.in_context || r.inject_hit)), bb((r) => r.in_context || r.inject_hit),
          pct(share(a, (r) => r.in_context || r.auto_hit || r.pull8_hit)), bb((r) => r.in_context || r.auto_hit || r.pull8_hit)]);
      });
    });
    table(el, ["probe set", "asked", "n", "in context", "in the summary", "+ gated injection, static", "+ gated injection, BGE", "+ full injection, static", "+ full injection, BGE", "+ one search, static", "+ one search, BGE"], rows, [2, 3, 4, 5, 6, 7, 8, 9, 10]);
  }

  function gate() {
    const el = $("#gate-table");
    const g = D.gate;
    if (!el || !g || !g.policy) return;
    const rows = [];
    Object.entries(g.policy).forEach(([path, p]) => {
      const name = /locomo/.test(path) ? "LoCoMo (held out)" : "LongMemEval-S (fit)";
      rows.push([name, "always inject 4,000 characters", pct(p.always_coverage), pct(p.always_precision), "100%", "≈ 3,000"]);
      rows.push(["", { v: "gated: full, brief or nothing", cls: "sys" }, pct(p.coverage), pct(p.precision), pct(p.inject_rate), fmt(p.chars_per_prompt, 0)]);
    });
    table(el, ["benchmark", "policy", "questions with evidence injected", "injections that hold evidence", "prompts injected", "characters per prompt"], rows, [2, 3, 4, 5]);
  }

  function scale() {
    const el = $("#scale-table");
    const sc = D.scale;
    if (!el || !sc) return;
    const rows = sc.map((r) => [
      r.records.toLocaleString("en-US"),
      fmt(r.embed_records_per_s / 1000, 1) + "k",
      fmt(r.bulk_load_s, 1),
      fmt(r.bytes_per_record / 1000, 2),
      ms(r.append_ms_p50),
      ms(r.query_ms_p50) + " / " + ms(r.query_ms_p99),
      ms(r.hook_ms_p50) + " / " + ms(r.hook_ms_p99),
      fmt(r.hook_first_ms, 0),
      num(r.recall10, 2),
      num(r.pack4k, 2),
    ]);
    table(el, ["records", "embedded per s", "bulk load s", "KB per record", "append ms (fsync)", "recall query ms p50 / p99", "hook ms p50 / p99 (process + daemon)", "first hook ms (daemon start)", "needle R@10", "needle in injection"], rows, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
  }

  function live() {
    const el = $("#live-table");
    const L = D.live;
    if (!el || !L) return;
    const wrap = h("div", { class: "table-wrap compact wrap-head live" }, el);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    ["run", "prompt", "Claude's answer (excerpt)", "Engram", "cost"].forEach((x) => h("th", {}, hr, x));
    const tb = h("tbody", {}, t);
    L.runs.forEach((r) => {
      const tr = h("tr", {}, tb);
      h("td", { class: "sys" }, tr, r.name);
      h("div", { class: "q" }, h("td", {}, tr), r.prompt.length > 260 ? r.prompt.slice(0, 260) + "…" : r.prompt);
      const res = (r.result || "").replace(/\*\*|`{1,3}/g, "").replace(/\s+/g, " ");
      const td = h("td", {}, tr);
      if (r.tools && r.tools.length) h("div", { class: "q" }, td, "tools: " + r.tools.map((x) => x.name.replace("mcp__engram__", "")).join(", "));
      h("div", { class: "r" }, td, res.length > 650 ? res.slice(0, 650) + "…" : res);
      h("td", {}, tr, r.engram ? "on" : "off");
      h("td", { class: "n" }, tr, "$" + (r.cost_usd || 0).toFixed(4));
    });
  }

  function liveQA() {
    const el = $("#live-qa-table");
    const L = D.live_qa;
    if (!el || !L) return;
    const qs = [...new Set(L.runs.map((r) => r.q))];
    const wrap = h("div", { class: "table-wrap compact wrap-head live" }, el);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    ["question", "Engram, round 1", "Engram, round 2 (asked again)", "Engram off"].forEach((x) => h("th", {}, hr, x));
    const tb = h("tbody", {}, t);
    const cell = (tr, r) => {
      const td = h("td", {}, tr);
      if (!r) return;
      const how = r.engram ? `${r.decision === "Inject" ? "full injection" : r.decision === "Brief" ? "brief injection" : "no injection"} (G ${fmt(r.gate || 0, 2)})${r.tools && r.tools.some((x) => /memory_/.test(x)) ? " · tools: " + r.tools.filter((x) => /memory_/.test(x)).join(", ") : ""}` : "no memory";
      h("div", { class: "q" }, td, how);
      const res = (r.result || "").replace(/\*\*|`{1,3}/g, "").replace(/\s+/g, " ");
      h("div", { class: "r" }, td, res.length > 300 ? res.slice(0, 300) + "…" : res);
    };
    qs.forEach((q) => {
      const tr = h("tr", {}, tb);
      h("td", { class: "sys" }, tr, q);
      cell(tr, L.runs.find((r) => r.q === q && r.round === 1));
      cell(tr, L.runs.find((r) => r.q === q && r.round === 2));
      cell(tr, L.runs.find((r) => r.q === q && !r.engram));
    });
  }

  meta();
  liveQA();
  retrieval();
  typeTable();
  ctxTable();
  probes();
  probeTable();
  gate();
  scale();
  live();
})();
