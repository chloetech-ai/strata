// Charts and tables of the Strata report, drawn from DATA (scripts/build_report.py).
(function () {
  "use strict";
  const { s, h, fmt, ms, css, showTip, hideTip, tipRows } = window.PB;
  const $ = (q) => document.querySelector(q);
  const D = DATA;
  if (!D) return;
  const COLLS = ["scifact", "fiqa", "trec-covid", "quora"];
  const NAME = { scifact: "SciFact", fiqa: "FiQA", "trec-covid": "TREC-COVID", quora: "Quora" };
  const SYS = [
    { key: "strata", name: "Strata", color: "--s1" },
    { key: "sqlite", name: "SQLite + FTS5", color: "--s2" },
    { key: "tantivy", name: "Tantivy", color: "--s4" },
  ];
  const KIND = {
    bm25: "BM25 top-10",
    and2: "AND, 2 terms",
    and3: "AND, 3 terms",
    or3: "OR, 3 terms",
    filter: "structured filter",
    filtered_bm25: "filtered BM25",
    facet: "facet counts",
    vector: "vector top-10",
    filtered_vector: "filtered vector",
    hybrid: "hybrid (RRF)",
    filtered_hybrid: "filtered hybrid",
  };
  const KINDS = Object.keys(KIND);
  const C = (c) => D.collections[c] || {};
  const kindOf = (sys, k) => sys && (sys.kinds || []).find((r) => r.kind === k);
  const mib = (b) => b / (1 << 20);
  const num = (v, d) => (v == null || !isFinite(v) ? "—" : v.toFixed(d));

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
      const y0 = f.m.t + i * band + 5;
      s("text", { x: f.m.l - 8, y: y0 + band / 2 - 2, "text-anchor": "end", class: "lab" }, g, cat);
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

  // ------------------------------------------------------------ sections
  function meta() {
    const m = $("#meta-line");
    if (m) m.textContent = "Intel Xeon (Emerald Rapids), 4 cores, AVX-512 · single-threaded queries · Rust, -C target-cpu=native · SQLite 3.x bundled with FTS5 · Tantivy 0.26";
    const f = $("#footer");
    if (f) f.textContent = `Generated from strata/results/ by scripts/build_report.py · ${D.generated}`;
  }

  function buildTable() {
    const el = $("#build-table");
    if (!el) return;
    const rows = [];
    COLLS.forEach((c) => {
      const n = C(c).strata && C(c).strata.n_docs;
      SYS.forEach((sy) => {
        const b = C(c)[sy.key] && C(c)[sy.key].build;
        if (!b) return;
        rows.push([sy === SYS[0] ? NAME[c] : "", { v: sy.name, cls: "sys" }, num(b.seconds, 1), num(mib(b.bytes), 0), n ? num(b.bytes / n, 0) : "—"]);
      });
    });
    table(el, ["collection", "system", "load s", "MiB", "bytes / doc"], rows, [2, 3, 4]);
  }

  function sizeTable() {
    const el = $("#size-table");
    if (!el) return;
    const groups = { "text (body)": /^text/, "structured": /^structured/, "vectors (float32)": /f32/, "vectors (SQ8)": /sq8/, "vector index": /^vectors \((hnsw|ivf|graph|lists|meta)/, "doc store": /^doc store/, "keys": /^pk/ };
    const rows = COLLS.map((c) => {
      const b = C(c).strata && C(c).strata.build;
      if (!b) return null;
      const comp = Object.fromEntries(b.components);
      const out = Object.fromEntries(Object.keys(groups).map((k) => [k, 0]));
      let other = 0;
      Object.entries(comp).forEach(([name, bytes]) => {
        const g = Object.keys(groups).find((k) => groups[k].test(name));
        if (g) out[g] += bytes; else other += bytes;
      });
      out["vector index"] += other;
      return [NAME[c], ...Object.keys(groups).map((k) => num(mib(out[k]), 1)), num(mib(b.bytes), 0)];
    }).filter(Boolean);
    table(el, ["MiB", ...Object.keys(groups), "file"], rows, [1, 2, 3, 4, 5, 6, 7, 8]);
  }

  function latency() {
    const el = $("#latency-chart");
    if (!el) return;
    const series = SYS.map((sy) => ({ ...sy, color: css(sy.color) }));
    legend(el, series);
    let mode = "warm", coll = "trec-covid";
    const draw = () => {
      const kinds = KINDS.filter((k) => SYS.some((sy) => kindOf(C(coll)[sy.key], k)));
      const ser = series.map((sy) => ({
        name: sy.name,
        color: sy.color,
        missing: sy.key === "tantivy" ? "no vector search" : "not supported",
        values: kinds.map((k) => {
          const r = kindOf(C(coll)[sy.key], k);
          const lat = r && (mode === "warm" ? r.warm : r.cold);
          return lat ? lat.p50_ms : null;
        }),
      }));
      logBars(el, kinds.map((k) => KIND[k]), ser, { xTitle: `${mode} latency, median ms per query (log scale)`, valueFmt: (v) => ms(v) + " ms", labelWidth: 140 });
    };
    tabs(el, COLLS.filter((c) => C(c).strata).map((c) => [c, NAME[c]]), (c) => { coll = c; draw(); }, "trec-covid");
    const modeBar = h("div", { class: "tabs" });
    el.parentNode.insertBefore(modeBar, el);
    [["warm", "warm cache"], ["cold", "cold cache"]].forEach(([m, label]) => {
      const b = h("button", { type: "button", "aria-pressed": String(m === mode) }, modeBar, label);
      b.addEventListener("click", () => {
        mode = m;
        modeBar.querySelectorAll("button").forEach((x) => x.setAttribute("aria-pressed", String(x === b)));
        draw();
      });
    });
    window.addEventListener("resize", draw);
  }

  function speedupTable() {
    const el = $("#speedup-table");
    if (!el) return;
    const rows = KINDS.map((k) => {
      const cells = [KIND[k]];
      COLLS.forEach((c) => {
        const st = kindOf(C(c).strata, k), sq = kindOf(C(c).sqlite, k), tv = kindOf(C(c).tantivy, k);
        const r = (o) => (st && o ? o.warm.p50_ms / st.warm.p50_ms : null);
        const a = r(sq), b = r(tv);
        cells.push(a == null ? "—" : `${fmt(a, 1)}×`, b == null ? "—" : `${fmt(b, 1)}×`);
      });
      return cells;
    });
    const head = ["kind"];
    COLLS.forEach((c) => head.push(`${NAME[c]} vs SQLite`, `vs Tantivy`));
    table(el, head, rows, [1, 2, 3, 4, 5, 6, 7, 8]);
  }

  function quality() {
    const el = $("#quality-table");
    if (!el) return;
    const nd = (sys, k) => { const r = kindOf(sys, k); return r && r.ndcg10 != null ? r.ndcg10 : null; };
    const rows = [];
    const cell = (v, best) => ({ v: num(v, 3), cls: v != null && best != null && Math.abs(v - best) < 1e-9 ? "best" : "" });
    COLLS.forEach((c) => {
      const hy = C(c).hybrid && C(c).hybrid.methods;
      const wbest = hy ? Math.max(...Object.entries(hy).filter(([m]) => m.startsWith("weighted_")).map(([, v]) => v.ndcg10)) : null;
      const wname = hy ? Object.entries(hy).filter(([m]) => m.startsWith("weighted_")).sort((a, b) => b[1].ndcg10 - a[1].ndcg10)[0][0].replace("weighted_", "") : "";
      const vals = [nd(C(c).strata, "bm25"), nd(C(c).sqlite, "bm25"), nd(C(c).tantivy, "bm25"), nd(C(c).sqlite, "vector"), nd(C(c).strata, "vector"), nd(C(c).sqlite, "hybrid"), nd(C(c).strata, "hybrid"), wbest];
      const best = Math.max(...vals.filter((v) => v != null));
      rows.push([NAME[c], ...vals.map((v) => cell(v, best)), wname ? `α = ${wname}` : "—"]);
    });
    table(el, ["collection", "BM25 Strata", "BM25 FTS5", "BM25 Tantivy", "vector exact", "vector HNSW", "RRF SQLite", "RRF Strata", "weighted, best α", "best α (BM25 weight)"], rows, [1, 2, 3, 4, 5, 6, 7, 8]);
  }

  function hybridSweep() {
    const el = $("#hybrid-sweep");
    if (!el) return;
    const alphas = [0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1];
    const colors = ["--s1", "--s2", "--s3", "--s7"];
    const series = COLLS.filter((c) => C(c).hybrid).map((c, i) => {
      const m = C(c).hybrid.methods;
      const pick = (a) => (a === 0 ? m.vector : a === 1 ? m.bm25 : m[`weighted_${a.toFixed(1)}`]);
      return { name: NAME[c], color: css(colors[i]), points: alphas.map((a) => [a, pick(a) ? pick(a).ndcg10 : null]).filter((p) => p[1] != null) };
    });
    const rrf = COLLS.filter((c) => C(c).hybrid).map((c, i) => ({ name: `${NAME[c]} RRF`, color: css(colors[i]), dash: "5 4", width: 1.4, r: 0, points: [[0, C(c).hybrid.methods.rrf.ndcg10], [1, C(c).hybrid.methods.rrf.ndcg10]] }));
    legend(el, series.concat([{ name: "dashed: RRF of the same two rankings", color: css("--ink-3") }]));
    const draw = () => xy(el, series.concat(rrf), { xTitle: "weight of BM25 (vector gets 1 − weight)", yTitle: "nDCG@10", xMin: 0, xMax: 1, yMin: 0.2, xTicks: alphas, xFmt: (v) => v.toFixed(1), yFmt: (v) => v.toFixed(2) });
    draw();
    window.addEventListener("resize", draw);
  }

  function vectorCurves() {
    const el = $("#vector-curves");
    if (!el) return;
    const kinds = [["hnsw", "HNSW (ef sweep)", "--s1"], ["ivf", "IVF (nprobe sweep)", "--s3"], ["flat", "flat SQ8 + re-rank", "--s2"]];
    legend(el, kinds.map(([, n, c]) => ({ name: n, color: css(c) })));
    let coll = "quora";
    const draw = () => {
      const v = C(coll).vector;
      if (!v) return el.replaceChildren(h("p", {}, null, "No vector results for this collection."));
      const series = kinds.map(([k, n, c]) => {
        const ix = v.indexes.find((x) => x.kind === k);
        const pts = ix ? ix.curve.map((p) => [p.p50_us / 1000, p.recall10, p.ef ? `ef ${p.ef}` : p.nprobe ? `nprobe ${p.nprobe}` : `re-rank ${p.rerank}`]) : [];
        return { name: n, color: css(c), points: pts };
      });
      xy(el, series, { logX: true, xTitle: "median latency, ms (log)", yTitle: "recall@10", yMin: Math.min(0.5, ...series.flatMap((sr) => sr.points.map((p) => p[1]))), yMax: 1.005, yFmt: (v) => v.toFixed(2) });
    };
    tabs(el, ["fiqa", "trec-covid", "quora"].filter((c) => C(c).vector).map((c) => [c, NAME[c]]), (c) => { coll = c; draw(); }, "quora");
    window.addEventListener("resize", draw);
  }

  function filtered() {
    const el = $("#filtered-chart");
    if (!el) return;
    const plans = [["prefilter", "pre-filter scan", "--s2"], ["index+probe", "HNSW in-filter", "--s3"], ["postfilter+probe", "post-filter", "--s4"], ["oracle", "best plan per query", "--ink-3"], ["auto", "planner", "--s1"]];
    legend(el, plans.map(([, n, c]) => ({ name: n, color: css(c) })));
    let coll = "quora";
    const draw = () => {
      const p = C(coll).planner && C(coll).planner.filtered_vector;
      if (!p) return el.replaceChildren(h("p", {}, null, "No planner results for this collection."));
      const by = {};
      p.rows.forEach((r) => {
        const t = r.target;
        by[t] = by[t] || {};
        Object.entries(r.plans).forEach(([k, [m]]) => (by[t][k] = by[t][k] || []).push(m));
        const qualifying = Object.entries(r.plans).filter(([k, [, rec]]) => k !== "auto" && rec >= 0.9).map(([, [m]]) => m);
        if (qualifying.length) (by[t].oracle = by[t].oracle || []).push(Math.min(...qualifying));
      });
      const ts = Object.keys(by).map(Number).sort((a, b) => a - b);
      const mean = (a) => a.reduce((x, y) => x + y, 0) / a.length;
      const series = plans.map(([k, n, c]) => ({ name: n, color: css(c), width: k === "auto" ? 3 : 2, dash: k === "oracle" ? "4 3" : "0", points: ts.filter((t) => by[t][k]).map((t) => [t, mean(by[t][k])]) }));
      xy(el, series, { logX: true, logY: true, xTicks: ts, xTitle: "filter selectivity (log)", yTitle: "mean ms per query (log)", xFmt: (v) => (v * 100 >= 1 ? (v * 100).toFixed(0) : (v * 100).toFixed(1)) + "%", yFmt: (v) => tick(v) });
    };
    tabs(el, COLLS.filter((c) => C(c).planner).map((c) => [c, NAME[c]]), (c) => { coll = c; draw(); }, "quora");
    window.addEventListener("resize", draw);
  }

  function plannerTable() {
    const el = $("#planner-table");
    if (!el) return;
    const label = { filtered_vector: "filtered vector", filtered_bm25: "filtered BM25", filter: "structured filter (count)" };
    const rows = [];
    const groups = [];
    COLLS.forEach((c) => groups.push([NAME[c], C(c).planner]));
    COLLS.forEach((c) => groups.push([`${NAME[c]}, IVF`, C(c).planner_ivf]));
    groups.forEach(([name, p]) => {
      if (!p) return;
      Object.keys(label).forEach((k, i) => {
        const x = p[k];
        if (!x) return;
        const fixed = Object.entries(x.total_ms).filter(([n]) => n !== "auto").sort((a, b) => a[1] - b[1])[0];
        rows.push([i === 0 ? name : "", label[k], x.queries, num(x.total_ms.auto, 0), num(x.oracle_ms, 0), `${fixed[0]} ${num(fixed[1], 0)}`, num(x.regret_geomean, 2) + "×", num(100 * x.within_1_25, 0) + "%", num(x.auto_recall_mean, 3)]);
      });
    });
    table(el, ["collection", "queries", "n", "planner ms", "oracle ms", "best fixed plan, ms", "regret (geo-mean)", "within 1.25× of best", "planner recall@10"], rows, [2, 3, 4, 6, 7, 8]);
  }

  function storage() {
    const el = $("#wal-chart");
    const st = D.storage && D.storage.wal;
    if (!el || !st) return;
    const res = st.results;
    const series = [true, false].map((sync, i) => ({
      name: sync ? "fsync at commit" : "no fsync",
      color: css(i ? "--s4" : "--s1"),
      points: res.filter((r) => r.sync === sync).map((r) => [r.txn_docs, r.docs_per_s, `${fmt(r.commits_per_s, 0)} commits/s`]),
    }));
    legend(el, series);
    const draw = () => xy(el, series, { logX: true, logY: true, xTitle: "documents per transaction (log)", yTitle: "documents per second (log)", height: 280, xTicks: [1, 10, 100, 1000] });
    draw();
    window.addEventListener("resize", draw);
  }

  function textTable() {
    const el = $("#text-table");
    if (!el) return;
    const rows = [];
    ["fiqa", "trec-covid", "quora"].forEach((c) => {
      const t = C(c).text;
      if (!t) return;
      t.formats.forEach((f, i) => {
        const w = f.warm;
        const bool = (op) => (w.boolean || []).find((b) => b.op === op);
        const bm = (algo) => (w.bm25 || []).find((b) => b.algo === algo && b.filter == null);
        const us = (x) => (x ? num(x.mean_us, x.mean_us < 100 ? 1 : 0) : "—");
        rows.push([i === 0 ? NAME[c] : "", { v: f.format === "loom" ? "Loom (default)" : "BP128 blocks", cls: "sys" }, num(f.sizes.bits_per_posting.postings_section, 2), us(bool("and")), us(bool("or")), us(bm("maxscore")), us(bm("bmw")), us(bm("exhaustive"))]);
      });
    });
    table(el, ["collection", "posting format", "bits / posting", "AND", "OR", "BM25 MaxScore", "BM25 Block-Max WAND", "BM25 exhaustive"], rows, [2, 3, 4, 5, 6, 7]);
  }

  // Cold-cache latency under three page-cache policies (TREC-COVID, Quora).
  function ablationTable() {
    const el = $("#ablation-table");
    if (!el) return;
    const cfg = [["readahead", "read-ahead everywhere"], ["strata", "default hints"], ["noreadahead", "no read-ahead"]];
    const colls = ["trec-covid", "quora"].filter((c) => C(c).readahead && C(c).noreadahead);
    const kinds = ["bm25", "and2", "filter", "filtered_bm25", "vector", "filtered_vector", "hybrid", "facet"];
    const wrap = h("div", { class: "table-wrap compact wrap-head ablation" }, el);
    const t = h("table", {}, wrap);
    const th = h("thead", {}, t);
    const r1 = h("tr", {}, th);
    h("th", {}, r1, "");
    colls.forEach((c) => h("th", { colspan: String(cfg.length), class: "grp" }, r1, NAME[c]));
    const r2 = h("tr", {}, th);
    h("th", {}, r2, "kind");
    colls.forEach(() => cfg.forEach(([, n]) => h("th", { class: "n" }, r2, n)));
    const tb = h("tbody", {}, t);
    kinds.forEach((k) => {
      const tr = h("tr", {}, tb);
      h("td", {}, tr, KIND[k]);
      colls.forEach((c) => {
        const rs = cfg.map(([key]) => kindOf(C(c)[key], k));
        const vals = rs.map((r) => (r && r.cold ? r.cold.p50_ms : null));
        const best = Math.min(...vals.filter((v) => v != null));
        rs.forEach((r, i) => {
          const td = h("td", { class: "n" + (vals[i] === best ? " best" : "") }, tr);
          if (vals[i] == null) { td.textContent = "—"; return; }
          h("div", {}, td, ms(vals[i]));
          h("div", { class: "pg" }, td, `${Math.round(r.cold_pages.p50).toLocaleString("en-US")} pages`);
        });
      });
    });
  }

  // The compound SQL queries, their latency per collection and the plan chosen.
  function showcaseTable() {
    const el = $("#showcase-table");
    if (!el) return;
    const colls = COLLS.filter((c) => C(c).showcase);
    if (!colls.length) return;
    const tag = (plan) => {
      const t = [];
      plan.forEach((line) => {
        const l = line.replace(/^seg \d+: /, "");
        let m;
        if ((m = l.match(/^selectivity ([\d.]+) global, ([\d.]+) near/))) t.push(`s ${fmt(100 * m[1], 0)}% · near ${fmt(100 * m[2], 0)}%`);
        else if ((m = l.match(/^filter materialised, (\d+) docs/))) t.push(`set of ${(+m[1]).toLocaleString("en-US")}`);
        else if ((m = l.match(/^bm25 \w+, filter (\w+)/))) t.push(m[1] === "Probe" ? "BM25, probe filter" : "BM25 over set");
        else if ((m = l.match(/^vector (\w+) \((\w+) ran\)/))) t.push({ PreFilter: "vector pre-filter", PostFilter: "vector post-filter", InIndex: m[2] === "Hnsw" ? "HNSW in-filter" : "IVF in-filter", Flat: "flat scan" }[m[1]] || m[1]);
        else if ((m = l.match(/^facet (\w+)/))) t.push(m[1] === "Codes" ? "facet by codes" : "facet by postings");
        else if (/^count from the range index/.test(l)) t.push("count from histogram");
      });
      return t.join(" → ");
    };
    const wrap = h("div", { class: "table-wrap compact wrap-head showcase" }, el);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    h("th", {}, hr, "query");
    colls.forEach((c) => h("th", {}, hr, `${NAME[c]}: ms, plan`));
    const tb = h("tbody", {}, t);
    C(colls[0]).showcase.queries.forEach((q, i) => {
      const tr = h("tr", {}, tb);
      const td = h("td", {}, tr);
      h("div", { class: "sc-label" }, td, q.label);
      h("code", { class: "sc-sql" }, td, q.sql);
      colls.forEach((c) => {
        const r = C(c).showcase.queries[i];
        const cell = h("td", {}, tr);
        h("div", { class: "sc-ms" }, cell, `${ms(r.ms)} ms · ${r.rows} row${r.rows === 1 ? "" : "s"}`);
        h("div", { class: "sc-plan" }, cell, tag(r.plan));
      });
    });
  }

  meta();
  ablationTable();
  showcaseTable();
  textTable();
  buildTable();
  sizeTable();
  latency();
  speedupTable();
  quality();
  hybridSweep();
  vectorCurves();
  filtered();
  plannerTable();
  storage();
})();
