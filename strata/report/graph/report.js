// Tables and charts of the graph report, drawn from DATA
// (scripts/build_graph_report.py).
(function () {
  "use strict";
  const { h, css, lines, legend } = window.PB;
  const $ = (q) => document.querySelector(q);
  const D = DATA;
  if (!D) return;
  const num = (v, d) => (v == null || !isFinite(v) ? "—" : v.toFixed(d));
  const int = (v) => (v == null ? "—" : Math.round(v).toLocaleString("en-US"));
  const msf = (v) => (v == null ? "—" : v < 0.1 ? v.toFixed(3) : v < 10 ? v.toFixed(2) : v.toFixed(1));
  const mb = (b) => (b / 1e6).toFixed(1) + " MB";

  function table(container, head, rows, numeric, opts) {
    if (!container) return;
    container.replaceChildren();
    const wrap = h("div", { class: "table-wrap" + (opts && opts.cls ? " " + opts.cls : "") }, container);
    const t = h("table", {}, wrap);
    const tr = h("tr", {}, h("thead", {}, t));
    head.forEach((c, i) => h("th", numeric.includes(i) ? { class: "num" } : {}, tr, c));
    const tb = h("tbody", {}, t);
    rows.forEach((r) => {
      const row = h("tr", {}, tb);
      r.forEach((c, i) => {
        const cell = h("td", numeric.includes(i) ? { class: "num" } : {}, row);
        if (c && typeof c === "object") {
          cell.textContent = c.v;
          if (c.cls) cell.className = (cell.className + " " + c.cls).trim();
        } else cell.textContent = c;
      });
    });
  }

  function tabs(container, items, onPick) {
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
    pick(items[0][0]);
  }

  function sizeTable() {
    const L = D.load;
    if (!L) return;
    const c = L.sizes.components;
    const order = ["vectors", "doc store", "graph", "text", "primary key", "columns"];
    const names = { vectors: "vectors (float32, SQ8, HNSW)", "doc store": "doc store (title, abstract, edges as stored JSON)", graph: "graph section", text: "full-text index", "primary key": "primary key", columns: "columns (year, category)" };
    const rows = order.filter((k) => c[k] != null).map((k) => [{ v: names[k], cls: k === "graph" ? "sys" : "" }, mb(c[k]), num((100 * c[k]) / L.sizes.file_bytes, 1) + "%", k === "graph" ? num(c[k] / L.edges, 1) : ""]);
    rows.push([{ v: "file", cls: "sys" }, mb(L.sizes.file_bytes), "100%", ""]);
    table($("#size-table"), ["arXiv table, " + int(L.papers) + " rows, " + int(L.edges) + " edges", "size", "share", "bytes per edge"], rows, [1, 2, 3], { cls: "compact" });
  }

  const QNAMES = {
    reach_out_1: "REACHABLE, 1 hop out",
    reach_out_2: "REACHABLE, 2 hops out",
    reach_out_3: "REACHABLE, 3 hops out",
    reach_in_1: "REACHABLE, 1 hop in",
    reach_both_1: "REACHABLE, 1 hop both ways",
    reach_both_2: "REACHABLE, 2 hops both ways",
    reach_both_2_top20: "the same, top 20 by hops() with ids",
    path_out: "PATH along the edges",
    path_both: "PATH either way",
    "ppr_1e-4": "ppr, ε = 10⁻⁴, top 20",
    "ppr_1e-5": "ppr, ε = 10⁻⁵, top 20",
    vector_top20: "vector search alone, top 20",
    vector_seeds_expand: "vector seeds → neighbours, by similarity",
    rrf_vector_ppr: "rrf(similarity, ppr from vector seeds)",
    semantic_knn_1hop: "similarity graph, 1 hop of 10",
    semantic_knn_2hop: "similarity graph, 2 hops of 10",
  };

  function speedTable(el, R) {
    const p = (o) => (o ? `${msf(o.p50)} / ${msf(o.p95)}` : "—");
    const rows = Object.keys(QNAMES).filter((k) => R.results[k]).map((k) => {
      const r = R.results[k];
      const vsLite = r.sqlite_ms ? num(r.sqlite_ms.p95 / r.strata_ms.p95, 1) + "×" : "";
      const vsCsr = r.csr_ms ? num(r.strata_ms.p50 / r.csr_ms.p50, 1) + "×" : "";
      const extra = r.rows != null ? int(r.rows) + " rows" : r.found != null ? `${r.found} of ${R.queries} found` : r.pushes != null ? int(r.pushes) + " pushes" : "";
      return [{ v: QNAMES[k], cls: "sys" }, extra, p(r.strata_ms), p(r.sqlite_ms), p(r.csr_ms), vsLite, vsCsr];
    });
    table(el, ["query (200 seeds)", "result", "Strata p50 / p95 ms", "SQLite p50 / p95 ms", "in-memory CSR p50 / p95 ms", "SQLite ÷ Strata, p95", "Strata ÷ CSR, p50"], rows, [2, 3, 4, 5, 6], { cls: "wrap-head compact" });
  }

  function speed() {
    const el = $("#speed-tabs");
    if (!el) return;
    const sets = [["arxiv", "arXiv: 168k rows, 1.15M edges, 2 segments"], ["synthetic", "power law: 1M rows, 10M edges, 4 segments"]].filter(([k]) => D.speed[k]);
    tabs(el, sets, (k) => speedTable(el, D.speed[k]));
  }

  function speedHistory() {
    const el = $("#speed-history");
    const H = D.history;
    if (!el || !H) return;
    const rows = [];
    for (const [set, label] of [["arxiv", "arXiv"], ["synthetic", "power law, 1M"]]) {
      const a = H[set] && H[set].before, b = H[set] && H[set].after;
      if (!a || !b) continue;
      for (const k of ["reach_out_3", "reach_both_2", "path_both", "ppr_1e-4", "ppr_1e-5"]) {
        if (!a[k] || !b[k]) continue;
        rows.push([{ v: label, cls: "sys" }, QNAMES[k], msf(a[k].strata_ms.p50), msf(b[k].strata_ms.p50), num(a[k].strata_ms.p50 / b[k].strata_ms.p50, 1) + "×", b[k].csr_ms ? msf(b[k].csr_ms.p50) : "—"]);
      }
    }
    table(el, ["graph", "query", "first version, p50 ms", "now, p50 ms", "faster by", "CSR, p50 ms"], rows, [2, 3, 4, 5], { cls: "wrap-head compact" });
  }

  const FAMILY = {
    vector: "vector search",
    bm25: "BM25",
    hybrid: "hybrid: rrf(BM25, vector)",
    "cited by vector seeds, by similarity": "papers the seeds cite, by similarity",
    "linked to vector seeds either way, by similarity": "papers linked to the seeds either way, by similarity",
    "ppr from vector seeds": "PageRank from vector seeds",
    "rrf(vector, ppr from vector seeds)": "rrf(vector, PageRank from vector seeds)",
    "rrf(bm25, vector, ppr from hybrid seeds)": "rrf(BM25, vector, PageRank from hybrid seeds)",
    "semantic kNN graph from vector seeds": "similarity graph (10 nearest) from vector seeds",
  };
  const COLORS = ["--s1", "--s2", "--s3", "--s4", "--s5", "--s6"];

  function qualityTable() {
    const el = $("#quality-table");
    const Q = D.quality;
    if (!el || !Q) return;
    const ms = Object.entries(Q.methods).sort((a, b) => b[1].ndcg - a[1].ndcg);
    const base = Q.methods.vector;
    const rows = ms.map(([k, r]) => {
      const graph = r.S != null;
      const edges = !graph ? "no" : k.startsWith("semantic") ? "no: similarity edges" : "yes";
      const cls = k === "cited by vector seeds, by similarity" ? "good" : graph ? "" : "sys";
      return [{ v: FAMILY[k] || k, cls }, edges, r.S == null ? "" : String(r.S), num(r.recall, 3), num(r.ndcg, 3), (r.ndcg >= base.ndcg ? "+" : "") + num((100 * (r.ndcg - base.ndcg)) / base.ndcg, 0) + "%", msf(r.ms)];
    });
    table(el, ["method (report half, " + Q.queries.test + " papers)", "uses citations", "seeds S (chosen on the other half)", "recall@20", "nDCG@20", "nDCG vs vector search", "ms per query"], rows, [2, 3, 4, 5, 6], { cls: "wrap-head compact" });
  }

  function sweep() {
    const el = $("#sweep-chart");
    const Q = D.quality;
    if (!el || !Q) return;
    const fams = ["cited by vector seeds, by similarity", "linked to vector seeds either way, by similarity", "ppr from vector seeds", "rrf(bm25, vector, ppr from hybrid seeds)", "rrf(vector, ppr from vector seeds)", "semantic kNN graph from vector seeds"];
    const xs = [1, 2, 3, 5, 10, 20, 50];
    const series = fams.filter((f) => Q.sweep[f]).map((f, i) => ({ name: FAMILY[f], color: css(COLORS[i % COLORS.length]), values: xs.map((x) => (Q.sweep[f][String(x)] ? Q.sweep[f][String(x)].test_ndcg : null)) }));
    for (const [f, c] of [["vector", "--ink-3"], ["hybrid", "--ink-2"]]) {
      const v = Q.sweep[f] && Q.sweep[f]["None"] ? Q.sweep[f]["None"].test_ndcg : null;
      if (v != null) series.push({ name: FAMILY[f], color: css(c), values: xs.map(() => v) });
    }
    legend(el, series);
    lines(el, xs, series, { logX: true, xTitle: "vector seeds S", yTitle: "nDCG@20", xLabel: "S = ", valueFmt: (v) => v.toFixed(3), height: 320 });
  }

  function compareTable() {
    const el = $("#compare-table");
    const Q = D.quality;
    if (!el || !Q) return;
    const rows = [];
    for (const base of ["vector", "hybrid", "bm25"]) {
      const best = Object.entries(Q.methods).filter(([k, r]) => r.S != null).sort((a, b) => b[1].ndcg - a[1].ndcg);
      for (const [k] of best) {
        const c = Q.comparisons[`${k} vs ${base} (ndcg)`];
        const r = Q.comparisons[`${k} vs ${base} (recall)`];
        if (!c) continue;
        const p = c.p < 0.001 ? "< 0.001" : num(c.p, 3);
        rows.push([{ v: FAMILY[k] || k, cls: c.diff > 0 ? "" : "sys" }, FAMILY[base], (c.diff >= 0 ? "+" : "") + num(c.diff, 3), `${num(c.ci95[0], 3)} to ${num(c.ci95[1], 3)}`, (c.relative >= 0 ? "+" : "") + num(100 * c.relative, 0) + "%", r ? (r.diff >= 0 ? "+" : "") + num(r.diff, 3) : "—", p]);
      }
    }
    table(el, ["graph method", "against", "Δ nDCG@20", "95% interval", "relative", "Δ recall@20", "p (sign-flip)"], rows, [2, 3, 4, 5, 6], { cls: "wrap-head compact" });
  }

  function meta() {
    const el = $("#meta-line");
    if (el) el.textContent = `Generated ${D.generated} · ogbn-arxiv and a synthetic power-law graph · 4-core machine, warm cache · results in strata/results/graph`;
  }

  meta();
  sizeTable();
  speed();
  speedHistory();
  qualityTable();
  sweep();
  compareTable();
  let last = window.innerWidth;
  window.addEventListener("resize", () => {
    if (Math.abs(window.innerWidth - last) < 40) return;
    last = window.innerWidth;
    document.querySelectorAll(".legend").forEach((l) => l.remove());
    sweep();
  });
})();
