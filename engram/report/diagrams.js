// Diagrams of the Engram report (architecture, live window, gate tiers).
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

  function arch() {
    const g = $("#arch-diagram");
    if (!g) return;
    // Claude Code
    s("rect", { x: 0, y: 4, width: 200, height: 332, rx: 8, class: "box" }, g);
    s("text", { x: 12, y: 24, class: "tb" }, g, "Claude Code session");
    const events = [
      ["SessionStart", "briefing in"],
      ["UserPromptSubmit", "prompt out, recall in"],
      ["PreToolUse Edit/Write", "file notes in"],
      ["PostToolUse", "call + result out"],
      ["Stop", "answer, narration out"],
      ["Pre/PostCompact", "summary out"],
      ["SessionEnd", "flush"],
    ];
    events.forEach(([e, d], i) => {
      const y = 40 + i * 34;
      s("rect", { x: 10, y, width: 180, height: 28, rx: 4, class: i === 1 ? "box-acc" : "box" }, g);
      s("text", { x: 18, y: y + 12, class: "tm" }, g, e);
      s("text", { x: 18, y: y + 24, class: "ts" }, g, d);
    });
    s("rect", { x: 10, y: 284, width: 180, height: 42, rx: 4, class: "box-warm" }, g);
    s("text", { x: 18, y: 300, class: "tm" }, g, "MCP: memory_search,");
    s("text", { x: 18, y: 316, class: "tm" }, g, "timeline, sql, remember");
    // clients
    box(g, 240, 70, 150, 58, "box", "engram hook <event>", ["one process per hook", "~10 ms, never blocks"]);
    box(g, 240, 276, 150, 50, "box", "engram mcp", ["stdio JSON-RPC"]);
    arrow(g, 200, 99, 240, 99);
    arrow(g, 200, 305, 240, 305);
    s("text", { x: 300, y: 196, class: "ts" }, g, "unix socket,");
    s("text", { x: 300, y: 210, class: "ts" }, g, "one JSON line each way");
    arrow(g, 390, 99, 450, 120);
    arrow(g, 390, 301, 450, 280);
    // daemon
    s("rect", { x: 450, y: 4, width: 310, height: 276, rx: 8, class: "box-acc" }, g);
    s("text", { x: 462, y: 24, class: "tb" }, g, "engram serve (one per project)");
    s("text", { x: 462, y: 40, class: "ts" }, g, "started on demand, exits when idle");
    box(g, 462, 52, 138, 92, "box", "capture", ["chunk ≤ 1,500 chars", "redact secrets", "salience prior ι", "episode cards"]);
    box(g, 610, 52, 138, 92, "box", "embed", ["static Model2Vec,", "512-d, µs per record", "or BGE-small (384-d)"]);
    box(g, 462, 154, 286, 64, "box", "recall", ["exclude live window · BM25 + vectors · priors", "gate: full / brief / none · pack and label"]);
    box(g, 462, 226, 286, 44, "box", "session state", ["window start, injected ids, turn, transcript offset"]);
    // strata
    s("rect", { x: 450, y: 296, width: 310, height: 40, rx: 6, class: "box-warm" }, g);
    s("text", { x: 462, y: 312, class: "tb" }, g, "Strata: memory.strata + WAL");
    s("text", { x: 462, y: 328, class: "ts" }, g, "table memory: text, ctx, kind, session, seq, ts, files, emb");
    arrow(g, 530, 144, 530, 154, "ln");
    arrow(g, 740, 280, 740, 296, "ln-acc");
    arrow(g, 470, 296, 470, 270, "ln");
    arrow(g, 450, 58, 202, 58, "ln-acc");
    s("text", { x: 262, y: 52, class: "ts" }, g, "additionalContext → model");
  }

  function windowDiagram() {
    const g = $("#window-diagram");
    if (!g) return;
    const y = 70;
    s("text", { x: 0, y: 14, class: "tl" }, g, "records in order of seq (project-wide)");
    const parts = [
      [0, 210, "box", "earlier sessions", "stored only: eligible"],
      [210, 470, "box", "this session, before the last compaction", "summarised, stored: eligible"],
      [470, 690, "box-acc", "live window", "in context: never injected"],
    ];
    parts.forEach(([x0, x1, cls, t, sub]) => {
      s("rect", { x: x0, y: y - 22, width: x1 - x0 - 4, height: 44, rx: 4, class: cls }, g);
      s("text", { x: x0 + 8, y: y - 4, class: "tb" }, g, t);
      s("text", { x: x0 + 8, y: y + 12, class: "ts" }, g, sub);
    });
    s("path", { d: `M466 ${y - 34} L466 ${y + 34}`, class: "ln-warm" }, g);
    s("text", { x: 400, y: y + 50, class: "ts" }, g, "compaction: w_s");
    s("path", { d: `M694 ${y - 34} L694 ${y + 34}`, class: "ln" }, g);
    s("text", { x: 700, y: y + 4, class: "ts" }, g, "prompt");
    [60, 150, 300, 380].forEach((x, i) => {
      s("circle", { cx: x, cy: y + 30, r: 4, class: "dot-acc" }, g);
      s("path", { d: `M${x} ${y + 34} C ${x} ${y + 90}, 600 ${y + 70}, 640 ${y + 100}`, class: "ln-acc", opacity: 0.6 }, g);
    });
    box(g, 560, y + 100, 200, 34, "box-warm", "injected with the prompt", "");
    s("text", { x: 0, y: y + 122, class: "ts" }, g, "dots: retrieved records, outside the window and not already injected since w_s");
  }

  function gateDiagram() {
    const g = $("#gate-diagram");
    if (!g) return;
    const x0 = 20, x1 = 740, y = 54;
    const G = (v) => x0 + ((v - 2) / (11 - 2)) * (x1 - x0);
    const zones = [
      [2, 5.329, "box", "nothing", "0 tokens"],
      [5.329, 7.658, "box-warm", "brief: 2 records", "≤ 1,200 characters"],
      [7.658, 11, "box-acc", "full: up to 8 records", "≤ 4,000 characters"],
    ];
    zones.forEach(([a, b, cls, t, sub]) => {
      s("rect", { x: G(a), y: y - 26, width: G(b) - G(a) - 3, height: 48, rx: 4, class: cls }, g);
      s("text", { x: G(a) + 8, y: y - 8, class: "tb" }, g, t);
      s("text", { x: G(a) + 8, y: y + 9, class: "ts" }, g, sub);
    });
    s("path", { d: `M${x0} ${y + 34} L${x1} ${y + 34}`, class: "axis" }, g);
    [[5.329, "θ_lo = 5.33 (precision 0.7)"], [7.658, "θ_hi = 7.66 (precision 0.9)"]].forEach(([v, t]) => {
      s("path", { d: `M${G(v)} ${y + 28} L${G(v)} ${y + 40}`, class: "ln" }, g);
      s("text", { x: G(v) - 4, y: y + 56, class: "ts", "text-anchor": "middle" }, g, t);
    });
    s("text", { x: x1, y: y + 56, class: "ts", "text-anchor": "end" }, g, "G →");
  }

  arch();
  windowDiagram();
  gateDiagram();
})();
