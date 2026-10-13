// Diagrams of the graph report: a segment's graph section and the query
// flow.
(function () {
  "use strict";
  const { s } = window.PB;
  const $ = (q) => document.querySelector(q);

  function box(g, x, y, w, h, cls, title, sub) {
    s("rect", { x, y, width: w, height: h, rx: 6, class: cls }, g);
    if (title) s("text", { x: x + 10, y: y + (sub ? 19 : h / 2 + 4.5), class: "tb" }, g, title);
    if (sub) (Array.isArray(sub) ? sub : [sub]).forEach((line, i) => s("text", { x: x + 10, y: y + 35 + i * 14, class: "ts" }, g, line));
  }
  function arrow(g, x1, y1, x2, y2, cls) {
    s("path", { d: `M${x1} ${y1} L${x2} ${y2}`, class: cls || "ln" }, g);
    const a = Math.atan2(y2 - y1, x2 - x1);
    const p = (r, t) => `${x2 - r * Math.cos(a - t)} ${y2 - r * Math.sin(a - t)}`;
    s("path", { d: `M${x2} ${y2} L${p(7, 0.45)} L${p(7, -0.45)} Z`, class: cls === "ln-acc" ? "ah" : "ahm" }, g);
  }
  // A row of array cells with a label.
  function cells(g, x, y, label, values, cls, w) {
    s("text", { x, y: y - 6, class: "tl" }, g, label);
    values.forEach((v, i) => {
      s("rect", { x: x + i * (w || 30), y, width: (w || 30) - 3, height: 22, rx: 3, class: cls || "box" }, g);
      s("text", { x: x + i * (w || 30) + ((w || 30) - 3) / 2, y: y + 15, class: "tm", "text-anchor": "middle" }, g, v);
    });
  }

  function storage() {
    const g = $("#storage-diagram");
    if (!g) return;
    s("rect", { x: 0, y: 0, width: 500, height: 330, rx: 8, class: "box" }, g);
    s("text", { x: 12, y: 20, class: "tb" }, g, "segment 2 · section graph.cites");
    // Out CSR.
    s("text", { x: 12, y: 44, class: "ts" }, g, "out-edges (CSR over local docIDs)");
    cells(g, 12, 66, "offsets", ["0", "2", "2", "5"], "box", 32);
    cells(g, 160, 66, "target key", ["p7", "p9", "p2", "p7", "q4"], "box-acc", 46);
    cells(g, 160, 112, "label · weight", ["—", "ext", "—", "—", "cit"], "box", 46);
    cells(g, 160, 158, "same-segment hint", ["14", "—", "3", "14", "—"], "box-warm", 46);
    s("text", { x: 12, y: 104, class: "ts" }, g, "doc 0: edges 0..2");
    s("text", { x: 12, y: 118, class: "ts" }, g, "doc 2: edges 2..5");
    // Reverse index.
    s("text", { x: 12, y: 214, class: "ts" }, g, "reverse index (grouped by target key, ordered by hash)");
    cells(g, 12, 236, "radix dir", ["·", "·", "·", "·"], "box", 26);
    cells(g, 130, 236, "groups: key", ["p2", "p7", "p9", "q4"], "box-acc", 46);
    cells(g, 330, 236, "sources", ["2", "0", "2", "0", "2"], "box", 30);
    s("text", { x: 12, y: 290, class: "ts" }, g, "who points at p7? one directory read, one key check, then sources 0 and 2 in sequence");
    s("text", { x: 12, y: 306, class: "ts" }, g, "(hash collisions make separate groups; integer keys need no check)");
    // Resolution.
    arrow(g, 395, 77, 540, 60, "ln-acc");
    box(g, 540, 30, 220, 74, "box-acc", "hint row still live?", ["yes: that row is the target", "(a key has one live row)"]);
    arrow(g, 650, 104, 650, 132);
    box(g, 540, 132, 220, 74, "box", "else: memo, then key lookup", ["this query's resolved keys,", "then each segment's pk index"]);
    arrow(g, 650, 206, 650, 234);
    box(g, 540, 234, 220, 74, "box-warm", "snapshot", ["disk segments 1, 2, 3 and", "memtable segments; live rows only"]);
  }

  function flow() {
    const g = $("#flow-diagram");
    if (!g) return;
    box(g, 0, 18, 150, 82, "box-acc", "seeds", ["keys, a tuple, or a", "subquery: vector, BM25,", "filters, LIMIT"]);
    box(g, 0, 130, 150, 82, "box", "graph options", ["dir, label, through,", "hops, alpha, eps"]);
    arrow(g, 150, 60, 186, 100, "ln-acc");
    arrow(g, 150, 170, 186, 130);
    box(g, 186, 66, 170, 98, "box-warm", "traversal", ["over the whole snapshot:", "BFS, bidirectional path,", "or PageRank push;", "edges resolved by key"]);
    arrow(g, 356, 115, 392, 115, "ln-acc");
    box(g, 392, 52, 180, 126, "box-acc", "per segment", ["document set with exact", "cardinality (REACHABLE,", "PATH), plus each row's", "hops and PageRank score", "(hops(), ppr())"]);
    arrow(g, 572, 115, 608, 115, "ln-acc");
    box(g, 608, 40, 152, 150, "box", "usual plan", ["filter: probe or", "materialise; vector", "search over a small set", "= exact pre-filtered", "scan; rrf, weighted", "fusion, facets, COUNT"]);
    s("text", { x: 0, y: 232, class: "ts" }, g, "A seed subquery runs as a query of its own on the same snapshot; through-predicates are compiled per segment and probed during the traversal.");
  }

  storage();
  flow();
})();
