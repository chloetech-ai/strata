// Diagrams of the Strata report (architecture, file, indexes, plan).
(function () {
  "use strict";
  const { s } = window.PB;
  const $ = (q) => document.querySelector(q);

  function box(g, x, y, w, h, cls, title, sub) {
    s("rect", { x, y, width: w, height: h, rx: 6, class: cls }, g);
    if (title) s("text", { x: x + 10, y: y + (sub ? 19 : h / 2 + 4.5), class: "tb" }, g, title);
    if (sub) {
      (Array.isArray(sub) ? sub : [sub]).forEach((line, i) => s("text", { x: x + 10, y: y + 35 + i * 14, class: "ts" }, g, line));
    }
  }
  function arrow(g, x1, y1, x2, y2, cls) {
    s("path", { d: `M${x1} ${y1} L${x2} ${y2}`, class: cls || "ln" }, g);
    const a = Math.atan2(y2 - y1, x2 - x1);
    const p = (r, t) => `${x2 - r * Math.cos(a - t)} ${y2 - r * Math.sin(a - t)}`;
    s("path", { d: `M${x2} ${y2} L${p(7, 0.45)} L${p(7, -0.45)} Z`, class: cls === "ln-acc" ? "ah" : "ahm" }, g);
  }

  function arch() {
    const g = $("#arch-diagram");
    if (!g) return;
    box(g, 0, 6, 220, 46, "box", "SQL text", "SELECT … ORDER BY bm25(…) LIMIT 10");
    arrow(g, 220, 29, 262, 29);
    box(g, 262, 6, 220, 46, "box", "parser + binder", "sqlparser AST → schema fields");
    arrow(g, 482, 29, 524, 29);
    box(g, 524, 6, 236, 46, "box", "bound query", "predicate tree · score · projection");
    arrow(g, 642, 52, 642, 78);
    box(g, 120, 78, 640, 50, "box-acc", "planner (per segment)", "materialise or probe · range scan or postings · vector strategy · local selectivity");
    box(g, 0, 78, 108, 50, "box", "statistics", ["df, histograms,", "list sizes, costs"]);
    arrow(g, 108, 103, 120, 103);
    const ops = [
      ["text operators", ["MaxScore / BMW top-k", "boolean, phrase, prefix"]],
      ["structured operators", ["range scan / postings", "keyword lists, facets"]],
      ["vector operators", ["flat, IVF, HNSW", "pre / in / post-filter"]],
      ["fusion and output", ["RRF, weighted, top-k", "merge, projection"]],
    ];
    ops.forEach(([t, sub], i) => {
      box(g, i * 192, 154, 184, 62, i === 3 ? "box-warm" : "box", t, sub);
      arrow(g, i * 192 + 92, 128, i * 192 + 92, 154);
    });
    s("text", { x: 0, y: 244, class: "tl" }, g, "segments (immutable; zero-copy views)");
    const secs = ["text.body", "col / kw / range", "vec.emb", "store, pk"];
    for (let k = 0; k < 3; k++) {
      const x0 = k * 200;
      s("rect", { x: x0, y: 252, width: 190, height: 70, rx: 6, class: "box" }, g);
      s("text", { x: x0 + 10, y: 268, class: "ts" }, g, k < 2 ? `segment ${k + 1} (on disk)` : "memtable (in memory)");
      secs.forEach((name, j) => {
        s("rect", { x: x0 + 8 + j * 45, y: 276, width: 42, height: 38, rx: 3, class: k < 2 ? "box-acc" : "box-warm" }, g);
        name.split(" ").slice(0, 2).forEach((w, r) => s("text", { x: x0 + 29 + j * 45, y: 292 + r * 12, class: "ts", "text-anchor": "middle" }, g, w));
      });
    }
    s("text", { x: 610, y: 280, class: "ts" }, g, "per-segment results");
    s("text", { x: 610, y: 294, class: "ts" }, g, "are merged; BM25 uses");
    s("text", { x: 610, y: 308, class: "ts" }, g, "collection-wide statistics");
    arrow(g, 300, 216, 300, 250);
    s("text", { x: 0, y: 352, class: "tl" }, g, "one file");
    const cells = [["A", "box-acc", 34], ["B", "box", 34], ["segment 1", "box-acc", 170], ["segment 2", "box-acc", 120], ["manifest", "box", 70], ["deletes", "box", 60], ["free", "box", 70]];
    let x = 0;
    cells.forEach(([t, cls, w]) => {
      s("rect", { x, y: 360, width: w - 4, height: 34, rx: 3, class: cls }, g);
      s("text", { x: x + (w - 4) / 2, y: 381, class: "ts", "text-anchor": "middle" }, g, t);
      x += w;
    });
    box(g, 600, 360, 160, 34, "box-warm", "", "");
    s("text", { x: 680, y: 381, class: "ts", "text-anchor": "middle" }, g, "-wal: log of writes");
    s("text", { x: 0, y: 414, class: "ts" }, g, "A, B: meta slots naming the current manifest · extents are page-aligned (4 KiB) · mmap'd read-only, written with pwrite");
  }

  function file() {
    const g = $("#file-diagram");
    if (!g) return;
    s("text", { x: 0, y: 16, class: "tl" }, g, "strata file (4 KiB pages)");
    const cells = [
      ["A", "box-acc", 40, "txn 8"], ["B", "box", 40, "txn 7"], ["segment 3 (new)", "box-acc", 150, ""], ["manifest 8", "box-acc", 86, ""],
      ["segment 1", "box", 130, ""], ["deletes 1", "box", 70, ""], ["manifest 7", "box", 86, ""], ["free", "box", 70, ""],
    ];
    let x = 0;
    const pos = {};
    cells.forEach(([t, cls, w, sub]) => {
      s("rect", { x, y: 26, width: w - 4, height: 40, rx: 3, class: cls }, g);
      s("text", { x: x + (w - 4) / 2, y: sub ? 43 : 50, class: "ts", "text-anchor": "middle" }, g, t);
      if (sub) s("text", { x: x + (w - 4) / 2, y: 58, class: "ts", "text-anchor": "middle" }, g, sub);
      pos[t] = x + (w - 4) / 2;
      x += w;
    });
    s("path", { d: `M${pos.A} 66 C ${pos.A} 104, ${pos["manifest 8"]} 104, ${pos["manifest 8"]} 68`, class: "ln-acc" }, g);
    s("path", { d: `M${pos.B} 66 C ${pos.B} 122, ${pos["manifest 7"]} 122, ${pos["manifest 7"]} 68`, class: "lnd" }, g);
    s("text", { x: pos["manifest 8"] + 40, y: 100, class: "ts" }, g, "current version");
    s("text", { x: pos["manifest 7"] + 46, y: 112, class: "ts" }, g, "previous version, still intact");
    const steps = [
      ["1", "write new extents (segment, deletes, manifest)"],
      ["2", "fsync"],
      ["3", "write the other meta slot: txn + 1, manifest, crc32"],
      ["4", "fsync: committed"],
    ];
    steps.forEach(([n, t], i) => {
      s("circle", { cx: 9, cy: 150 + i * 22, r: 8, class: "dot-acc" }, g);
      s("text", { x: 9, y: 154 + i * 22, class: "ts", "text-anchor": "middle", style: "fill: var(--surface)" }, g, n);
      s("text", { x: 24, y: 154 + i * 22, class: "tl" }, g, t);
    });
    box(g, 430, 140, 150, 64, "box-warm", "-wal", ["insert, delete,", "commit records + crc"]);
    arrow(g, 580, 172, 610, 172, "ln");
    box(g, 610, 140, 150, 64, "box-warm", "memtable", ["in-memory segments,", "1,024 docs each"]);
    s("path", { d: "M685 204 C 685 250, 300 250, 225 70", class: "lnd" }, g);
    s("text", { x: 430, y: 262, class: "ts" }, g, "flush when full: memtable → new segment (shadow-paging commit), WAL truncated");
    s("text", { x: 0, y: 292, class: "ts" }, g, "recovery: take the valid slot with the larger txn, then replay the WAL's committed transactions");
  }

  function indexes() {
    const g = $("#index-diagram");
    if (!g) return;
    const fields = [
      ["id", "KEYWORD, primary key"], ["title, text", "TEXT, stored"], ["body", "TEXT, generated"], ["year", "INT"], ["rating", "FLOAT"],
      ["category, topic", "KEYWORD"], ["emb", "VECTOR(1024)"],
    ];
    s("rect", { x: 0, y: 4, width: 220, height: 320, rx: 6, class: "box" }, g);
    s("text", { x: 12, y: 24, class: "tb" }, g, "a row of docs");
    const fy = {};
    fields.forEach(([f, t], i) => {
      const y = 50 + i * 40;
      s("text", { x: 14, y, class: "tm" }, g, f);
      s("text", { x: 14, y: y + 15, class: "ts" }, g, t);
      fy[f] = y - 4;
    });
    const targets = [
      ["text index", ["dictionary blocks of 32", "Loom docIDs + tf blocks", "positions · 1-byte norms"], 4, "box-acc"],
      ["structured index", ["FOR columns + zone maps", "keyword dict, codes, Pyramid lists", "range buckets (64, Pyramid)"], 92, "box-acc"],
      ["vector index", ["float32 + SQ8 codes", "HNSW graph or IVF clusters"], 180, "box-acc"],
      ["doc store · pk", ["LZ4 blocks of stored fields", "key hash → docID, docID → key"], 252, "box-warm"],
    ];
    const ty = {};
    targets.forEach(([t, sub, y, cls]) => {
      box(g, 470, y, 290, sub.length > 2 ? 74 : 62, cls, t, sub);
      ty[t] = y + 26;
    });
    const link = (f, t, cls) => s("path", { d: `M220 ${fy[f]} C 340 ${fy[f]}, 350 ${ty[t]}, 470 ${ty[t]}`, class: cls || "ln" }, g);
    link("body", "text index", "ln-acc");
    link("year", "structured index", "ln-acc");
    link("rating", "structured index", "ln-acc");
    link("category, topic", "structured index", "ln-acc");
    link("emb", "vector index", "ln-acc");
    link("title, text", "doc store · pk");
    link("id", "doc store · pk");
    s("path", { d: `M220 ${fy["title, text"]} C 250 ${fy["title, text"]}, 250 ${fy.body}, 220 ${fy.body}`, class: "lnd" }, g);
    s("text", { x: 232, y: (fy["title, text"] + fy.body) / 2 + 4, class: "ts" }, g, "generated");
  }

  function plan() {
    const g = $("#plan-diagram");
    if (!g) return;
    s("text", { x: 0, y: 14, class: "tm" }, g, "WHERE year >= 2015 AND category IN ('c01','c07')");
    s("text", { x: 0, y: 30, class: "tm" }, g, "ORDER BY 0.2 * bm25(body, :text) + 0.8 * similarity(emb, :q) DESC LIMIT 10");
    box(g, 280, 48, 200, 40, "box-warm", "top-10", "");
    arrow(g, 380, 108, 380, 90);
    box(g, 250, 108, 260, 46, "box-warm", "weighted fusion", "min-max per input · 0.2 / 0.8");
    arrow(g, 380, 172, 380, 156);
    box(g, 230, 172, 300, 46, "box", "exact re-score of the union", "BM25 via posting seeks · float32 dot");
    arrow(g, 210, 250, 300, 220);
    arrow(g, 550, 250, 460, 220);
    box(g, 60, 250, 260, 46, "box-acc", "BM25 MaxScore, top 100", "per segment, merged; global stats");
    box(g, 440, 250, 260, 46, "box-acc", "HNSW in-filter, top 100", "ef = 64; local selectivity checked");
    box(g, 250, 304, 260, 24, "box", "", "");
    s("text", { x: 380, y: 320, class: "ts", "text-anchor": "middle" }, g, "filter: probe (code bitset, then column)");
    s("path", { d: "M320 296 L320 304", class: "lnd" }, g);
    s("path", { d: "M440 296 L440 304", class: "lnd" }, g);
  }

  arch();
  file();
  indexes();
  plan();
})();
