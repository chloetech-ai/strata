// Diagrams of the record-structure report: a record's anatomy, the query
// path, and why small values cover more evidence under a budget.
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

  // A tool call becomes records; one record's fields by role.
  function anatomy() {
    const g = $("#anatomy-diagram");
    if (!g) return;
    box(g, 0, 10, 190, 74, "box", "PostToolUse: Bash", ["$ cargo test -p strata", "6,200 bytes of output", "turn 41: “fix the planner”"]);
    arrow(g, 95, 84, 95, 112);
    box(g, 0, 112, 190, 90, "box-warm", "capture", ["redact secrets", "keep head 4,500 B + tail 1,500 B", "cut at ≤ 400 B on line,", "sentence or word boundaries"]);
    arrow(g, 95, 202, 95, 228);
    [0, 1, 2].forEach((i) => {
      s("rect", { x: 12 + i * 8, y: 228 + i * 8, width: 166, height: 40, rx: 5, class: i === 2 ? "box-acc" : "box" }, g);
    });
    s("text", { x: 34, y: 262, class: "tb" }, g, "records, one per 400 B");
    arrow(g, 194, 260, 250, 160, "ln-acc");
    // One record, by role.
    s("rect", { x: 250, y: 4, width: 510, height: 300, rx: 8, class: "box" }, g);
    s("text", { x: 264, y: 26, class: "tb" }, g, "one record (one Strata row)");
    const rows = [
      ["box-acc", "lexical key", "body = text ⊕ ctx", ["BM25 with positions; ctx is the", "turn's request (≤ 200 B)"]],
      ["box-acc", "dense key", "emb = φ(text)", ["512-d static vector, SQ8 codes;", "HNSW on disk, a scan in memory"]],
      ["box-warm", "value", "text (≤ 400 B)", ["what is injected, under a header", "with id, time and kind"]],
      ["box", "attributes", "session · seq · turn · ts", ["live window, time ranges,", "timelines, the per-turn cap"]],
      ["box", "", "kind · tag · importance", ["briefing, priors ι,", "the uncertain filter"]],
      ["box", "", "files", ["pre-edit notes:", "MATCH(files, path)"]],
    ];
    rows.forEach(([cls, role, field, use], i) => {
      const y = 40 + i * 43;
      if (role) s("text", { x: 262, y: y + 23, class: "tl" }, g, role);
      s("rect", { x: 334, y, width: 196, height: 36, rx: 5, class: cls }, g);
      s("text", { x: 344, y: y + 22, class: "tm" }, g, field);
      use.forEach((line, k) => s("text", { x: 540, y: y + 15 + k * 13, class: "ts" }, g, line));
    });
  }

  // The prompt becomes three queries over the same rows.
  function queryPath() {
    const g = $("#query-diagram");
    if (!g) return;
    box(g, 0, 96, 150, 56, "box", "prompt q", ["the user's message", "(at most 2,000 chars)"]);
    const parts = [
      [10, "lexical query", "analysed terms of q", "BM25(body) top 50"],
      [92, "dense query", "φ(q)", "ANN over emb, top 50"],
      [174, "predicate P", "NOT (session = s", "AND seq ≥ w_s)"],
    ];
    parts.forEach(([y, t, a, b]) => {
      arrow(g, 150, 124, 196, y + 26);
      s("rect", { x: 196, y, width: 216, height: 58, rx: 6, class: t === "predicate P" ? "box-warm" : "box-acc" }, g);
      s("text", { x: 206, y: y + 19, class: "tb" }, g, t);
      s("text", { x: 206, y: y + 35, class: "tm" }, g, a);
      s("text", { x: 206, y: y + 50, class: t === "predicate P" ? "tm" : "ts" }, g, b);
    });
    arrow(g, 412, 39, 452, 110);
    arrow(g, 412, 121, 452, 121);
    arrow(g, 412, 203, 452, 132);
    box(g, 452, 88, 140, 66, "box", "fuse + priors", ["min-max BM25 ⊕ cosine", "× (1 + λι ι) + recency"]);
    arrow(g, 592, 121, 616, 121);
    box(g, 616, 74, 144, 94, "box-acc", "gate, then pack", ["G ≥ θhi: ≤ 4,000 chars", "θlo ≤ G < θhi: 3 records", "else nothing", "≤ 12 records, 8 per turn"]);
    s("text", { x: 196, y: 254, class: "ts" }, g, "The model's own tools add structured queries: memory_sql (kind, files, ts, session), memory_timeline (seq ± k).");
  }

  // The same budget filled with large or small values.
  function budget() {
    const g = $("#budget-diagram");
    if (!g) return;
    const W = 700, x0 = 60;
    const scale = W / 4000;
    const rowsDef = [
      ["1,500 B records", [[600, 1], [600, 0], [600, 0], [600, 0], [600, 0]], "5 snippets of 600 B: one holds the answer"],
      ["400 B records", [[330, 1], [400, 0], [260, 1], [380, 0], [300, 0], [400, 1], [350, 0], [280, 0], [310, 0]], "9 windows: three distinct answer-bearing spans"],
    ];
    rowsDef.forEach(([label, parts, note], r) => {
      const y = 22 + r * 74;
      s("text", { x: 0, y: y - 6, class: "tl" }, g, label);
      let x = x0;
      parts.forEach(([w, hit]) => {
        const hw = 80 * scale;
        s("rect", { x, y, width: hw, height: 26, class: "band" }, g);
        x += hw;
        s("rect", { x, y, width: w * scale - 2, height: 26, rx: 3, class: hit ? "box-acc" : "box" }, g);
        if (hit) s("circle", { cx: x + (w * scale) / 2, cy: y + 13, r: 4, class: "dot-acc" }, g);
        x += w * scale;
      });
      s("text", { x: x0, y: y + 44, class: "ts" }, g, note);
    });
    s("line", { x1: x0 + 4000 * scale, x2: x0 + 4000 * scale, y1: 10, y2: 168, class: "ln-warm" }, g);
    s("text", { x: x0 + 4000 * scale - 4, y: 178, class: "ts", "text-anchor": "end" }, g, "budget B = 4,000 chars (shaded: 80-char headers)");
  }

  // What a turn agreed becomes knowledge records, which reach later sessions
  // through the briefing and the per-prompt knowledge block.
  function knowledgeLoop() {
    const g = $("#knowledge-diagram");
    if (!g) return;
    box(g, 0, 70, 142, 92, "box", "end of a turn", ["Stop: the request,", "commands run, reply", "→ raw records,", "as before"]);
    arrow(g, 142, 116, 170, 116);
    box(g, 170, 52, 200, 128, "box-warm", "distill (small model)", ["after the hook has answered;", "sees the latest 40 notes:", "skips repeats, writes new;", "replaces: id → superseded;", "done: id → work closed;", "≈ $0.0013 and 8 s a turn"]);
    arrow(g, 370, 116, 398, 116, "ln-acc");
    [0, 1, 2].forEach((i) => s("rect", { x: 398 + i * 7, y: 62 + i * 7, width: 162, height: 98, rx: 6, class: i === 2 ? "box-acc" : "box" }, g));
    s("text", { x: 422, y: 96, class: "tb" }, g, "knowledge records");
    ["decision, with reason", "preference: a rule", "todo: planned work", "fact; near-copies dropped"].forEach((t, i) => s("text", { x: 422, y: 113 + i * 13, class: "ts" }, g, t));
    arrow(g, 574, 92, 594, 50, "ln-acc");
    arrow(g, 574, 140, 594, 182, "ln-acc");
    box(g, 594, 0, 166, 96, "box-acc", "session start", ["briefing ≤ 6,000 chars:", "planned, rules, decisions,", "facts, where it left off;", "“search memory first”"]);
    box(g, 594, 140, 166, 96, "box-acc", "every prompt", ["≤ 4 notes: cos ≥ 0.40", "or cov ≥ 0.30, not yet", "shown, whatever the gate;", "then gated raw recall"]);
    s("text", { x: 0, y: 236, class: "ts" }, g, "Raw records stay the source of truth;");
    s("text", { x: 0, y: 250, class: "ts" }, g, "knowledge records are keys a model writes over them.");
  }

  anatomy();
  queryPath();
  budget();
  knowledgeLoop();
})();
