(function () {
  "use strict";
  const { s, h, fmt, ms, css, scatter, hbars, lines, legend } = window.PB;
  const $ = (q) => document.querySelector(q);
  const NEW = ["Pyramid", "Loom", "Entropic"];
  const newColor = (name) => css(["--s1", "--s2", "--s3"][NEW.indexOf(name)]);
  const short = (n) => n.replace("Hybrid (bitmap + BP128)", "Hybrid").replace("BP128 (bitpacking crate)", "BP128 (crate)").replace("Roaring (roaring crate)", "Roaring (crate)");
  const isCount = (m) => m.algorithm.includes("count only");
  const best = (c, op) => c.measurements.filter((m) => m.op === op && !isCount(m)).sort((a, b) => a.ms_per_query - b.ms_per_query)[0];
  const countBest = (c, op) => c.measurements.filter((m) => m.op === op && isCount(m)).sort((a, b) => a.ms_per_query - b.ms_per_query)[0];
  const find = (run, name) => run && run.codecs.find((c) => c.codec === name);

  // ------------------------------------------------------------ diagrams
  function pyramidDiagram() {
    const g = $("#pyr-diagram");
    if (!g) return;
    // Chunk strip of 32 blocks (stand-in for 256) with presence / dense marks.
    const kinds = "ssd.dd.s..sdddd.s..s.ds..sddd..s".split("");
    const x0 = 120, bw = 18;
    s("text", { x: 0, y: 30, class: "tl" }, g, "chunk (2¹⁶ docIDs)");
    s("text", { x: 0, y: 46, class: "ts" }, g, "256 blocks of 256");
    kinds.forEach((k, i) => {
      const cls = k === "d" ? "box-acc" : k === "s" ? "box-warm" : "box";
      s("rect", { x: x0 + i * bw, y: 18, width: bw - 2, height: 30, rx: 3, class: cls }, g);
    });
    const mask = (pred) => kinds.map((k) => (pred(k) ? "1" : "0")).join("");
    s("text", { x: 0, y: 80, class: "tl" }, g, "present mask");
    s("text", { x: 0, y: 104, class: "tl" }, g, "dense mask");
    // One bit under each block, so the masks line up with the strip above.
    [[80, mask((k) => k !== ".")], [104, mask((k) => k === "d")]].forEach(([y, bits]) =>
      bits.split("").forEach((bit, i) =>
        s("text", { x: x0 + i * bw + (bw - 2) / 2, y, class: "tm", "text-anchor": "middle" }, g, bit)));
    s("text", { x: 760, y: 80, class: "ts", "text-anchor": "end" }, g, "4 × u64");
    s("text", { x: 760, y: 104, class: "ts", "text-anchor": "end" }, g, "4 × u64");
    // Leaf pools.
    s("text", { x: 0, y: 150, class: "tl" }, g, "dense pool");
    for (let i = 0; i < 9; i++) {
      s("rect", { x: x0 + i * 58, y: 132, width: 52, height: 26, rx: 3, class: "box-acc" }, g);
      s("text", { x: x0 + i * 58 + 26, y: 150, class: "ts", "text-anchor": "middle" }, g, "256 bits");
    }
    s("text", { x: 0, y: 196, class: "tl" }, g, "sparse pool");
    const lows = [[7, 90], [3], [41, 200], [18], [5, 6, 250], [77], [140, 141], [12]];
    let x = x0;
    lows.forEach((grp, gi) => {
      grp.forEach((v) => {
        s("rect", { x, y: 178, width: 28, height: 26, rx: 3, class: "box-warm" }, g);
        s("text", { x: x + 14, y: 196, class: "ts", "text-anchor": "middle" }, g, v);
        x += 30;
      });
      x += gi < lows.length - 1 ? 6 : 0;
    });
    s("text", { x: x0, y: 226, class: "ts" }, g, "one u8 per value (docID & 255), grouped by block; a small directory gives each block's offset");
    s("text", { x: 0, y: 252, class: "tl" }, g, "probe(x)");
    s("text", { x: x0, y: 252, class: "tm" }, g, "b = (x >> 8) & 255 → dense? bit test : SIMD compare of x & 255 with block b's lows");
  }

  function loomDiagram() {
    const g = $("#loom-grid");
    if (!g) return;
    const X0 = 60, W = 20, N = 34;
    for (let i = 0; i <= N; i++) s("line", { x1: X0 + i * W, x2: X0 + i * W, y1: 26, y2: 184, class: "gridl" }, g);
    s("text", { x: X0, y: 16, class: "ts" }, g, "64-docID words →");
    const rows = [
      { y: 40, label: "list A", parts: [[0, 7.4, "EF", "box"], [8, 21, "bitvector", "box-acc"], [21, 34, "EF", "box"]] },
      { y: 124, label: "list B", parts: [[0, 3.6, "EF", "box"], [3.6, 6.2, "run", "box-warm"], [6.2, 12, "EF", "box"], [12, 26, "bitvector", "box-acc"], [26, 34, "EF", "box"]] },
    ];
    s("rect", { x: X0 + 12 * W, y: 30, width: 9 * W, height: 150, rx: 6, class: "band" }, g);
    rows.forEach((r) => {
      s("text", { x: 0, y: r.y + 22, class: "tl" }, g, r.label);
      r.parts.forEach(([a, b, name, cls]) => {
        s("rect", { x: X0 + a * W + 1, y: r.y, width: (b - a) * W - 2, height: 36, rx: 4, class: cls }, g);
        s("text", { x: X0 + ((a + b) / 2) * W, y: r.y + 22, class: "tb", "text-anchor": "middle" }, g, name);
      });
    });
    s("text", { x: X0 + 16.5 * W, y: 104, class: "tb", "text-anchor": "middle" }, g, "A ∧ B: 9 word ANDs, no shifts");
    s("text", { x: X0, y: 206, class: "ts" }, g, "bitvector bit i ↔ docID 64·⌊base/64⌋ + i, so every list's bitvectors share one word grid");
    s("text", { x: X0, y: 226, class: "ts" }, g, "OR: per 2¹⁶-docID window, bitvectors OR word-wise, runs fill words, EF postings decode 16 at a time into bits");
    s("text", { x: X0, y: 246, class: "ts" }, g, "AND: per EF partition choose merge (ρ ≤ 12), forward probes, or far probes (ρ ≥ 64)");
  }

  function entropicDiagram() {
    const g = $("#ent-diagram");
    if (!g) return;
    const docs = [100, 101, 102, 105, 106, 140, 141, 142, 150, 151];
    const X0 = 110, W = 62;
    s("text", { x: 0, y: 30, class: "tl" }, g, "docIDs");
    s("text", { x: 0, y: 72, class: "tl" }, g, "gaps g");
    s("text", { x: 0, y: 114, class: "tl" }, g, "tokens t");
    const gaps = docs.map((d, i) => (i === 0 ? null : d - docs[i - 1] - 1));
    const tok = (gp) => (gp === null ? "" : gp <= 3 ? String(gp) : "w" + (32 - Math.clz32(gp)) + "·" + (((gp >> (31 - Math.clz32(gp) - 1)) & 1)));
    docs.forEach((d, i) => {
      const x = X0 + i * W;
      s("rect", { x, y: 12, width: W - 8, height: 26, rx: 4, class: "box" }, g);
      s("text", { x: x + (W - 8) / 2, y: 30, class: "tm", "text-anchor": "middle" }, g, d);
      if (i > 0) {
        s("text", { x: x + (W - 8) / 2, y: 72, class: "tm", "text-anchor": "middle" }, g, gaps[i]);
        const lane = (i - 1) % 2;
        s("rect", { x, y: 96, width: W - 8, height: 26, rx: 4, class: lane ? "box-warm" : "box-acc" }, g);
        s("text", { x: x + (W - 8) / 2, y: 114, class: "tm", "text-anchor": "middle" }, g, tok(gaps[i]));
        if (i > 2) {
          const xp = X0 + (i - 2) * W + (W - 8) / 2, xc = x + (W - 8) / 2;
          s("path", { d: `M${xp} 124 Q ${(xp + xc) / 2} ${lane ? 168 : 152} ${xc} 124`, class: lane ? "ln-warm" : "ln-acc" }, g);
        }
      }
    });
    s("text", { x: X0, y: 196, class: "ts" }, g, "blue: lane 0 (even tokens) · orange: lane 1 (odd tokens) · arcs: context = token two places back");
    s("text", { x: 0, y: 236, class: "tl" }, g, "block");
    const parts = [["skip: EF(max), EF(offset)", 210, "box"], ["2 lane states (bits-back)", 170, "box-acc"], ["tANS + raw bits, one stream", 270, "box-warm"]];
    let x = X0;
    parts.forEach(([t, w, cls]) => {
      s("rect", { x, y: 218, width: w - 6, height: 28, rx: 4, class: cls }, g);
      s("text", { x: x + 8, y: 237, class: "ts" }, g, t);
      x += w;
    });
  }

  // ------------------------------------------------------------ results
  function pts(run, op) {
    return run.codecs.map((c) => {
      const b = best(c, op);
      const isNew = NEW.includes(c.codec);
      return { label: short(c.codec), x: c.query_bits_per_posting, y: b.ms_per_query, algo: b.algorithm, color: isNew ? newColor(c.codec) : null, strong: isNew };
    });
  }

  function summary(container, run) {
    const wrap = h("div", { class: "table-wrap compact" }, container);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    ["codec", "index bits", "query bits", "aux MB", "decode ns", "AND ms", "", "OR ms", "", "OR count"].forEach((x, i) =>
      h("th", { class: [1, 2, 3, 4, 5, 7, 9].includes(i) ? "n" : "" }, hr, x));
    const tb = h("tbody", {}, t);
    const mins = {
      sp: Math.min(...run.codecs.map((c) => c.query_bits_per_posting)),
      and: Math.min(...run.codecs.map((c) => best(c, "AND").ms_per_query)),
      or: Math.min(...run.codecs.map((c) => best(c, "OR").ms_per_query)),
    };
    const shortAlgo = (a) => a.replace(" (NextGEQ)", "").replace(" (4096)", "").replace("slices: ", "").replace("adaptive (SIMD / gallop @64x)", "adaptive");
    run.codecs.forEach((c) => {
      const tr = h("tr", {}, tb);
      const a = best(c, "AND"), o = best(c, "OR"), oc = countBest(c, "OR");
      const td = h("td", { class: NEW.includes(c.codec) ? "strong" : "" }, tr, short(c.codec));
      if (NEW.includes(c.codec)) { const i = h("i", {}, null); i.style.cssText = `display:inline-block;width:9px;height:9px;border-radius:50%;margin-right:6px;background:${newColor(c.codec)}`; td.prepend(i); }
      h("td", { class: "n" }, tr, c.bits_per_posting.toFixed(2));
      h("td", { class: "n" + (c.query_bits_per_posting === mins.sp ? " best" : "") }, tr, c.query_bits_per_posting.toFixed(2));
      h("td", { class: "n" }, tr, c.aux_memory_bytes ? (c.aux_memory_bytes / 1e6).toFixed(1) : "—");
      h("td", { class: "n" }, tr, c.query_decode_ns_per_posting.toFixed(2));
      h("td", { class: "n" + (a.ms_per_query === mins.and ? " best" : "") }, tr, ms(a.ms_per_query));
      h("td", { class: "algo" }, tr, shortAlgo(a.algorithm));
      h("td", { class: "n" + (o.ms_per_query === mins.or ? " best" : "") }, tr, ms(o.ms_per_query));
      h("td", { class: "algo" }, tr, shortAlgo(o.algorithm));
      h("td", { class: "n" }, tr, oc ? ms(oc.ms_per_query) : "—");
    });
  }

  function newLegend(el) {
    if (el.previousElementSibling && el.previousElementSibling.classList.contains("legend")) return;
    const series = NEW.map((n) => ({ name: n, color: newColor(n) })).concat([{ name: "existing codecs", color: css("--ink-3") }]);
    legend(el, series, true);
  }

  const renderers = [];
  function chart(id, fn) {
    const el = document.getElementById(id);
    if (el) renderers.push(() => fn(el));
  }

  function runSection(key) {
    const run = DATA[key];
    if (!run) return;
    chart(key + "-and", (el) => { newLegend(el); scatter(el, pts(run, "AND"), { xTitle: "bits per posting, query lists (log)", yTitle: "AND ms/query (log)" }); });
    chart(key + "-or", (el) => { newLegend(el); scatter(el, pts(run, "OR"), { xTitle: "bits per posting, query lists (log)", yTitle: "OR ms/query (log)" }); });
    const t = document.getElementById(key + "-table");
    if (t) summary(t, run);
  }

  function ratioTable(container) {
    // New designs against the best existing codec in their target region.
    const wrap = h("div", { class: "table-wrap" }, container);
    const t = h("table", {}, wrap);
    const hr = h("tr", {}, h("thead", {}, t));
    ["", "collection", "size vs ref.", "AND vs ref.", "OR vs ref."].forEach((x, i) => h("th", { class: i > 1 ? "n" : "" }, hr, x));
    const tb = h("tbody", {}, t);
    const pairs = [["Pyramid", "Roaring"], ["Loom", "PEF"], ["Loom", "Roaring"], ["Entropic", "Interpolative"], ["Entropic", "PEF"]];
    pairs.forEach(([n, ref]) => {
      ["clustered", "uniform", "wikitext"].forEach((k) => {
        const a = find(DATA[k], n), b = find(DATA[k], ref);
        if (!a || !b) return;
        const tr = h("tr", {}, tb);
        h("td", { class: "strong" }, tr, `${n} vs ${ref}`);
        h("td", {}, tr, k === "wikitext" ? "WikiText-103" : k);
        const sp = a.query_bits_per_posting / b.query_bits_per_posting;
        const an = best(a, "AND").ms_per_query / best(b, "AND").ms_per_query;
        const or = best(a, "OR").ms_per_query / best(b, "OR").ms_per_query;
        [sp, an, or].forEach((r) => {
          const td = h("td", { class: "n " + (r <= 1 ? "ok" : "no") }, tr, (r <= 1 ? "" : "+") + ((r - 1) * 100).toFixed(0) + "%");
          td.title = r.toFixed(3) + "×";
        });
      });
    });
  }

  function init() {
    pyramidDiagram();
    loomDiagram();
    entropicDiagram();
    if (!DATA) return;
    ["clustered", "uniform", "wikitext", "selective", "dense"].forEach(runSection);
    const rt = $("#ratio-table");
    if (rt) ratioTable(rt);
    if (DATA.terms && Object.keys(DATA.terms).length) {
      chart("terms", (el) => {
        const ks = Object.keys(DATA.terms).map(Number).sort((a, b) => a - b);
        const pick = ["Pyramid", "Loom", "Entropic", "Roaring", "PEF", "BP128"];
        const cols = ["--s1", "--s2", "--s3", "--s4", "--s5", "--s6"];
        const series = pick.map((n, i) => ({
          name: n, color: css(cols[i]),
          values: ks.map((k) => { const c = find(DATA.terms[k], n); return c ? best(c, "AND").ms_per_query : null; }),
        })).filter((sr) => sr.values.some((v) => v != null));
        if (!el.previousElementSibling || !el.previousElementSibling.classList.contains("legend")) legend(el, series);
        lines(el, ks, series, { logY: true, xTitle: "terms per query", yTitle: "best AND ms/query (log)", xLabel: "k = ", valueFmt: ms });
      });
    }
    const meta = $("#meta-line");
    if (meta && DATA.clustered) meta.textContent = DATA.clustered.cpu + " · single thread · Rust, -C target-cpu=native";
    const f = $("#footer");
    if (f) f.textContent = "Generated from results/designs/*.json by scripts/build_report.py · " + (DATA.generated || "");
    const render = () => renderers.forEach((r) => r());
    render();
    let t;
    window.addEventListener("resize", () => { clearTimeout(t); t = setTimeout(render, 120); });
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    if (mq.addEventListener) mq.addEventListener("change", render);
    new MutationObserver(render).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init); else init();
})();
