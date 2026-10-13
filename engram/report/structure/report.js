// Charts and tables of the record-structure report, drawn from DATA
// (scripts/build_structure_report.py).
(function () {
  "use strict";
  const { s, h, css, lines, showTip, hideTip, tipRows } = window.PB;
  const $ = (q) => document.querySelector(q);
  const D = DATA;
  if (!D) return;
  const num = (v, d) => (v == null || !isFinite(v) ? "—" : v.toFixed(d));
  const pct = (v) => (v == null || !isFinite(v) ? "—" : (100 * v).toFixed(0) + "%");
  const int = (v) => (v == null ? "—" : Math.round(v).toLocaleString("en-US"));
  const msf = (v) => (v == null ? "—" : v < 1 ? v.toFixed(2) : v < 10 ? v.toFixed(1) : v.toFixed(0));

  function frame(container, height, m) {
    container.replaceChildren();
    const width = Math.max(300, container.clientWidth);
    const svg = s("svg", { class: "ch", width, height, viewBox: `0 0 ${width} ${height}` }, container);
    return { svg, width, height, iw: width - m.l - m.r, ih: height - m.t - m.b, m };
  }

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
    return bar;
  }

  function legendInto(el, series) {
    el.replaceChildren();
    series.forEach((sr) => {
      const sp = h("span", {}, el);
      const i = h("i", {}, sp);
      i.style.background = sr.color;
      sp.appendChild(document.createTextNode(sr.name));
    });
  }

  // Horizontal bars on a linear 0..max axis; labels above the bars when narrow.
  function bars(container, cats, series, opt) {
    const narrow = container.clientWidth < 560;
    const band = 14 * series.length + 12 + (narrow ? 16 : 0);
    const f = frame(container, cats.length * band + 46, { l: narrow ? 6 : opt.labelWidth || 200, r: 56, t: 6, b: 36 });
    const max = opt.max || 1;
    const xs = (v) => (v / max) * f.iw;
    const g = s("g", {}, f.svg);
    (narrow ? [0, max / 2, max] : opt.ticks || [0, 0.25, 0.5, 0.75, 1].map((t) => t * max)).forEach((v) => {
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
        if (v == null) return;
        const w = Math.max(1.5, xs(v));
        s("rect", { x: f.m.l, y, width: w, height: 12, rx: 2.5, fill: sr.colors ? sr.colors[i] : sr.color }, g);
        s("text", { x: f.m.l + w + 5, y: y + 10, class: "lab" }, g, opt.valueFmt(v));
        const hit = s("rect", { x: f.m.l, y: y - 1, width: Math.max(w, 30), height: 14, fill: "transparent" }, g);
        hit.addEventListener("pointermove", (e) => showTip(e, tipRows(cat, series.map((q) => [q.colors ? q.colors[i] : q.color, q.name, q.values[i] == null ? "n/a" : opt.valueFmt(q.values[i])]).concat(opt.extra ? opt.extra(i) : []))));
        hit.addEventListener("pointerleave", hideTip);
      });
    });
  }

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

  // ------------------------------------------------------------ layouts
  const NAMES = {
    "turn+req": "1,500 B, request in both keys (Engram before)",
    turn: "1,500 B, no context",
    "win400+req": "400 B, request in both keys",
    "win400+req/vec-text": "400 B, request in BM25 only (Engram now)",
    "win200+req": "200 B, request in both keys",
    "win800+req": "800 B, request in both keys",
    win400: "400 B, no context",
    "turn+partner": "1,500 B, both sides in both keys",
    "win400+partner": "400 B, both sides in both keys",
    "turn+win400+req": "1,500 B and 400 B rows together",
    "user-turns": "user turns only",
    session: "one row per session",
    facts: "facts only",
    "turn+facts": "turns and facts",
    "turn/fact-keys": "turns, facts in the key",
  };
  const COLORS = {
    "turn+req": "--ink-3", turn: "--s4", "win400+req": "--s3", "win400+req/vec-text": "--accent", "win200+req": "--s7",
    session: "--s8", "user-turns": "--s5", facts: "--s2", "turn+facts": "--s6", "turn/fact-keys": "--s5", "win800+req": "--s3",
  };
  const color = (l) => css(COLORS[l] || "--ink-2");

  // ------------------------------------------------------------ budget curves
  function budgetChart() {
    const el = $("#budget-chart");
    if (!el) return;
    const lg = $("#budget-legend");
    const datasets = [];
    if (D.lme) datasets.push(["lme", "LongMemEval-S"]);
    if (D.locomo) datasets.push(["locomo", "LoCoMo"]);
    if (D.coding) datasets.push(["coding_fresh", "Coding session: new session"], ["coding_compacted", "Coding session: after compaction"]);
    let ds = datasets[0][0];
    let metric = "ans";
    const metricBar = h("div", { class: "tabs", role: "group" });
    function draw() {
      const isCoding = ds.startsWith("coding");
      metricBar.hidden = isCoding;
      const B = isCoding ? D.coding.budgets : D[ds].budgets;
      let want = ["turn+req", "turn", "win400+req", "win400+req/vec-text", "win200+req", "session"];
      if (ds === "lme") want.push("user-turns");
      if (isCoding) want = want.filter((l) => l !== "session");
      const pres = (l) => (l === "user-turns" ? "span400+p150" : "snip600");
      const series = want.map((l) => {
        let values;
        if (isCoding) {
          const key = ds === "coding_fresh" ? "4633:5392|fresh" : "1952:4632|compacted";
          const p = D.coding.pooled[key] || {};
          values = B.map((b) => p[`${l}|${pres(l)}|${b}`]);
        } else {
          const m = metric === "all" ? "all_multi" : metric;
          values = B.map((b) => ((D[ds].pack[`${l}|${pres(l)}|${b}`] || {})[m]));
        }
        return { name: NAMES[l] + (l === "user-turns" ? " (+ reply head)" : ""), color: color(l), values };
      }).filter((sr) => sr.values.some((v) => v != null));
      const yTitle = isCoding ? "answer pattern in the context" : metric === "ans" ? "answer string in the context" : metric === "ev" ? "an evidence turn in the context" : "every evidence session in the context";
      lines(el, B, series, { xTitle: "injection budget (characters)", yTitle, logX: true, xTicks: B, xLabel: "budget ", valueFmt: (v) => v.toFixed(3), height: 330 });
      legendInto(lg, series);
      const sub = $("#budget-sub");
      if (sub) {
        if (isCoding) {
          const key = ds === "coding_fresh" ? "4633:5392|fresh" : "1952:4632|compacted";
          const e = D.coding.eligible[key] || {};
          const n = Object.values(e).reduce((a, b) => a + b, 0);
          sub.textContent = `${n} probe questions about this repository's 26-hour session (${Object.entries(e).map(([k, v]) => `${v} ${k.replace("session_probes", "recurring").replace("lost_probes_vague", "vague").replace("lost_probes", "lost")}`).join(", ")}) · answer counted when its pattern matches the packed text`;
        } else {
          const d = D[ds];
          sub.textContent = metric === "ans"
            ? `${d.n_verbatim} questions whose answer appears word for word in an evidence turn · ${d.units} conversations, one store each`
            : metric === "ev" ? `${d.n} answerable questions · at least 80 bytes of an evidence turn shown`
            : `${d.n_multi} questions with evidence in more than one session`;
        }
      }
    }
    [["ans", "answer in context"], ["ev", "evidence turn in context"], ["all", "all evidence sessions"]].forEach(([k, label]) => {
      const b = h("button", { type: "button", "aria-pressed": String(k === metric) }, metricBar, label);
      b.addEventListener("click", () => {
        metric = k;
        [...metricBar.children].forEach((c) => c.setAttribute("aria-pressed", String(c === b)));
        draw();
      });
    });
    tabs(el, datasets, (k) => { ds = k; draw(); });
    el.parentNode.insertBefore(metricBar, el);
  }

  // Ranking and budgeted coverage by layout.
  function layoutTable() {
    const el = $("#layout-table");
    if (!el || !D.lme) return;
    const order = ["turn+req", "turn", "turn+partner", "win800+req", "win400+req", "win400+req/vec-text", "win400", "win400+partner", "win200+req", "turn+win400+req", "user-turns", "session"];
    const rows = order.filter((l) => D.lme.rank[`${l}|hybrid`]).map((l) => {
      const r = D.lme.rank[`${l}|hybrid`];
      const p = (b) => D.lme.pack[`${l}|snip600|${b}`] || {};
      const lc = D.locomo && D.locomo.pack[`${l}|snip600|4000`];
      const cls = l === "win400+req/vec-text" ? "sys" : "";
      return [{ v: NAMES[l], cls }, int(D.lme.rows_per_unit[l]), num(r.r5, 3), num(r.r10, 3), num(r.mrr, 3), num(p(1000).ans, 3), num(p(4000).ans, 3), num(p(4000).ev, 3), lc && l !== "user-turns" ? num(lc.ans, 3) : "—"];
    });
    table(el, ["layout", "rows per history", "R@5", "R@10", "MRR", "answer @1,000", "answer @4,000", "evidence @4,000", "LoCoMo answer @4,000"], rows, [1, 2, 3, 4, 5, 6, 7, 8], { cls: "wrap-head" });
  }

  // Presentation (the value) for two layouts.
  function presTable() {
    const el = $("#pres-table");
    if (!el || !D.lme) return;
    const PN = { snip600: "best 600 B of the row", span400: "row widened to 400 B", span600: "row widened to 600 B", "span600+p150": "600 B + 150 B of the other side", "span400+p150": "400 B + 150 B of the other side", turn1500: "the whole turn (≤ 1,500 B)" };
    const rows = [];
    ["turn", "win400+req/vec-text"].forEach((l) => {
      D.lme.presentations.forEach((p, i) => {
        const at = (b) => D.lme.pack[`${l}|${p}|${b}`] || {};
        rows.push([i === 0 ? NAMES[l] : "", { v: PN[p] || p, cls: p === "snip600" && l === "win400+req/vec-text" ? "sys" : "" }, num(at(2000).ans, 3), num(at(4000).ans, 3), num(at(4000).ev, 3), num(at(4000).all_multi, 3)]);
      });
    });
    table(el, ["layout", "value shown", "answer @2,000", "answer @4,000", "evidence @4,000", "all sessions @4,000"], rows, [2, 3, 4, 5], { cls: "wrap-head" });
  }

  // ------------------------------------------------------------ answer quality
  const RN = {
    none: "no memory",
    today: "Engram before (1,500 B, request in both keys)",
    turn: "1,500 B, no context",
    turn_partner: "1,500 B, 400 B spans + other side",
    turn_partner_grouped: "same, grouped by conversation",
    turn_partner_8k: "1,500 B spans + other side, 8,000 chars",
    win400: "400 B, request in BM25 only (Engram now)",
    win400_2k: "400 B (Engram now), 2,000 chars",
    today_2k: "Engram before, 2,000 chars",
    oracle: "the evidence sessions, whole",
    full: "the whole history",
    facts: "facts only",
    turn_facts: "turns and facts",
    fact_keys: "turns, facts in the key",
  };
  const RC = { none: "--ink-3", today: "--s4", today_2k: "--s4", win400: "--accent", win400_2k: "--accent", turn: "--s7", turn_partner: "--s5", turn_partner_grouped: "--s5", turn_partner_8k: "--s5", oracle: "--s3", full: "--s2", facts: "--s6", turn_facts: "--s6", fact_keys: "--s6" };

  function accuracyChart() {
    const el = $("#accuracy-chart");
    if (!el || !D.reader) return;
    const order = ["none", "today_2k", "win400_2k", "today", "win400", "turn", "turn_partner", "turn_partner_grouped", "turn_partner_8k", "oracle"].filter((k) => D.reader[k]);
    const cats = order.map((k) => RN[k]);
    bars(el, cats, [{ name: "accuracy", colors: order.map((k) => css(RC[k] || "--ink-2")), values: order.map((k) => D.reader[k].acc) }], {
      xTitle: "share of the 500 questions answered correctly", valueFmt: (v) => v.toFixed(3), labelWidth: 290,
      extra: (i) => [[null, "context chars", int(D.reader[order[i]].chars)], [null, "reader tokens", int(D.reader[order[i]].tokens)]],
    });
  }

  function typeTable() {
    const el = $("#type-table");
    if (!el || !D.reader) return;
    const cfgs = ["none", "today", "win400", "oracle"].filter((k) => D.reader[k]);
    const types = ["single-session-user", "single-session-assistant", "single-session-preference", "knowledge-update", "temporal-reasoning", "multi-session", "abstention"];
    const rows = types.map((t) => {
      const n = (D.reader.today && D.reader.today.by_type[t] || {}).n;
      return [t, n].concat(cfgs.map((c) => num((D.reader[c].by_type[t] || {}).acc, 3)));
    });
    rows.push([{ v: "all", cls: "sys" }, (D.reader.today || {}).n].concat(cfgs.map((c) => ({ v: num(D.reader[c].acc, 3), cls: "sys" }))));
    table(el, ["question type", "n"].concat(cfgs.map((c) => RN[c])), rows, [1, 2, 3, 4, 5], { cls: "wrap-head" });
  }

  function longTable() {
    const el = $("#long-table");
    const d = D.reader_full_subset;
    if (!el || !d || !d.full) return;
    const cfgs = ["none", "today", "win400", "oracle", "full"].filter((k) => d[k]);
    const rows = cfgs.map((c) => [{ v: RN[c], cls: c === "win400" ? "sys" : "" }, d[c].n, num(d[c].acc, 3), int(d[c].tokens), "$" + d[c].cost.toFixed(4), d.win400 ? (d[c].tokens / d.win400.tokens).toFixed(1) + "×" : "—"]);
    table(el, ["context", "questions", "accuracy", "reader input tokens", "reader cost per question", "tokens vs Engram"], rows, [1, 2, 3, 4, 5], { cls: "wrap-head" });
  }

  function factsTable() {
    const el = $("#facts-table");
    const d = D.reader_facts_subset;
    const f = D.facts;
    if (!el || !d || !f) return;
    const map = [["today", "turn+req"], ["win400", "win400+req/vec-text"], ["turn", "turn"], ["facts", "facts"], ["turn_facts", "turn+facts"], ["fact_keys", "turn/fact-keys"]];
    const rows = map.filter(([c, l]) => d[c] || f.rank[`${l}|hybrid`]).map(([c, l]) => {
      const p = (b) => f.pack[`${l}|snip600|${b}`] || {};
      return [{ v: RN[c], cls: c === "win400" ? "sys" : "" }, num((f.rank[`${l}|hybrid`] || {}).r10, 3), num(p(2000).ev, 3), num(p(4000).ev, 3), num(p(4000).all_multi, 3), d[c] ? num(d[c].acc, 3) : "—", d[c] ? num((d[c].by_type["multi-session"] || {}).acc, 3) : "—"];
    });
    table(el, ["memory", "R@10", "evidence @2,000", "evidence @4,000", "all sessions @4,000", "accuracy", "multi-session accuracy"], rows, [1, 2, 3, 4, 5, 6], { cls: "wrap-head" });
  }

  // ------------------------------------------------------------ product
  function productTable() {
    const el = $("#product-table");
    if (!el) return;
    const r = D.retrieval || {};
    const row = (name, d) => {
      if (!d) return null;
      const a = d.all["hybrid_0.5_priors"];
      return [name, num(a["recall_any@5"], 3), num(a["recall_any@10"], 3), num(a.pack4k, 3), int(a.pack4k_chars), num(a.pack_brief, 3)];
    };
    const rows = [
      row("LongMemEval-S, before", r.longmemeval_s_static_request), row("LongMemEval-S, now", r.longmemeval_s_static_request_c400),
      row("LoCoMo, before", r.locomo_static_round), row("LoCoMo, now", r.locomo_static_request_c400),
    ].filter(Boolean);
    table(el, ["benchmark, Engram version", "R@5", "R@10", "evidence in 4,000 chars", "chars used", "evidence in the brief tier"], rows, [1, 2, 3, 4, 5], { cls: "wrap-head" });
  }

  function gateTable() {
    const el = $("#gate-table");
    const b = D.gate_before, a = D.gate_after;
    if (!el || !b || !a) return;
    const pol = (g, re) => Object.entries(g.policy || {}).find(([k]) => re.test(k));
    const rows = [];
    [["LongMemEval-S (fit)", /longmemeval/], ["LoCoMo (held out)", /locomo/]].forEach(([name, re]) => {
      [["before", b], ["now", a]].forEach(([when, g], i) => {
        const p = pol(g, re);
        if (!p) return;
        const v = p[1];
        rows.push([i === 0 ? name : "", { v: when, cls: when === "now" ? "sys" : "" }, pct(v.coverage), pct(v.precision), pct(v.inject_rate), int(v.chars_per_prompt), pct(v.always_coverage), pct(v.always_precision)]);
      });
    });
    table(el, ["benchmark", "Engram", "questions whose evidence is injected", "injections holding evidence", "prompts injected", "characters per prompt", "always inject: evidence", "always inject: precision"], rows, [2, 3, 4, 5, 6, 7], { cls: "wrap-head" });
  }

  function condTable() {
    const el = $("#cond-table");
    const c = D.conditional;
    if (!el || !c) return;
    const rows = [];
    [["today", "Engram before"], ["turn", "1,500 B, no context"], ["win400", "Engram now"]].forEach(([k, name]) => {
      const x = c[k];
      if (!x) return;
      const f = (m, v) => { const y = (x[m] || {})[v]; return y ? `${num(y.acc, 3)} (n = ${y.n})` : "—"; };
      rows.push([name, f("ans", "1"), f("ans", "0"), f("ev", "1"), f("ev", "0")]);
    });
    table(el, ["context", "answer string in it", "not in it", "an evidence turn in it", "none in it"], rows, [1, 2, 3, 4], { cls: "wrap-head" });
  }

  function probesTable() {
    const el = $("#probes-table");
    if (!el || !D.probes_before) return;
    const sets = [["probes", "recurring facts"], ["probes_lost", "lost details"], ["probes_vague", "lost details, vague"]];
    const rows = [];
    const stat = (rs, point, fresh, field) => {
      const sel = (rs || []).filter((x) => x.point === point && x.fresh === fresh && x.stored && (fresh || !x.in_context));
      if (!sel.length) return [null, 0];
      return [sel.filter((x) => x[field]).length, sel.length];
    };
    const cell = ([k, n]) => (k == null ? "—" : `${k} of ${n}`);
    sets.forEach(([key, label]) => {
      [["4633:5392", false, "after compaction"], ["4633:5392", true, "new session"]].forEach(([pt, fresh, when]) => {
        const b = D.probes_before[key], a = (D.probes_after || {})[key];
        rows.push([label, when, cell(stat(b, pt, fresh, "auto_hit")), cell(stat(a, pt, fresh, "auto_hit")), cell(stat(b, pt, fresh, "inject_hit")), cell(stat(a, pt, fresh, "inject_hit")), cell(stat(b, pt, fresh, "pull8_hit")), cell(stat(a, pt, fresh, "pull8_hit"))]);
      });
    });
    const tot = (when, field, which) => {
      let k = 0, n = 0;
      sets.forEach(([key]) => {
        const [pt, fresh] = when === "after compaction" ? ["4633:5392", false] : ["4633:5392", true];
        const [a, b] = stat((which === "before" ? D.probes_before : D.probes_after || {})[key], pt, fresh, field);
        k += a || 0; n += b;
      });
      return `${k} of ${n}`;
    };
    ["after compaction", "new session"].forEach((when) => {
      rows.push([{ v: "all", cls: "sys" }, when].concat(["auto_hit", "inject_hit", "pull8_hit"].flatMap((f) => [tot(when, f, "before"), { v: tot(when, f, "now"), cls: "sys" }])));
    });
    table(el, ["probe set", "when", "gated, before", "gated, now", "full injection, before", "full injection, now", "one search, before", "one search, now"], rows, [2, 3, 4, 5, 6, 7], { cls: "wrap-head compact" });
  }

  // ------------------------------------------------------------ latency
  function latencyTables() {
    const L = D.latency;
    if (!L || !L.length) return;
    const ph = $("#phase-table");
    if (ph) {
      const names = [["embed_query", "embed the prompt"], ["bm25_top50", "BM25 top 50"], ["vector_top50", "vector top 50"], ["idf_count_queries", "IDF by COUNT queries (before)"], ["idf_dictionary", "IDF from term dictionaries (now)"], ["fuse", "fuse, priors, gate features"], ["pack_format", "pack and format"], ["search_total", "search, end to end (now)"]];
      table(ph, ["phase (ms, median)"].concat(L.map((r) => int(r.records) + " records")), names.map(([k, label]) => [label].concat(L.map((r) => msf(((r.phases || {})[k] || {}).p50)))), L.map((_, i) => i + 1));
    }
    const st = $("#stall-table");
    if (st) {
      const cfgs = [["chunk1024_hnsw", "1,024 per chunk, graph (before)"], ["chunk1024_flat", "1,024 per chunk, scan"], ["chunk256_flat", "256 per chunk, scan (now)"], ["chunk64_flat", "64 per chunk, scan"]];
      const rows = [];
      L.forEach((r) => cfgs.forEach(([k, label], i) => {
        const c = (r.stall || {})[k];
        if (!c) return;
        rows.push([i === 0 ? int(r.records) : "", { v: label, cls: k === "chunk256_flat" ? "sys" : "" }, msf(c.append.p50), msf(c.rebuild.p50), msf(c.query_after_write.p50), msf(c.query_after_write.p99), msf(c.query_after_rebuild.p50)]);
      }));
      table(st, ["records", "memtable segments", "append (fsync)", "rebuild", "query right after a write", "p99", "query after the rebuild"], rows, [2, 3, 4, 5, 6], { cls: "wrap-head" });
    }
    const hk = $("#hook-table");
    if (hk) {
      const pre = D.latency_pre || [];
      const paths = [["noop", "an event Engram ignores"], ["tool", "PostToolUse (store a call)"], ["prompt", "UserPromptSubmit (recall)"]];
      const rows = [];
      L.forEach((r) => paths.forEach(([k, label], i) => {
        const p = (r.hooks || {})[`process_${k}`], c = (r.hooks || {})[`client_${k}`];
        const before = (pre.find((x) => x.records === r.records) || {}).hooks || {};
        if (!p || !c) return;
        rows.push([i === 0 ? int(r.records) : "", label, msf((before[`process_${k}`] || {}).p50), { v: msf(p.p50), cls: "sys" }, msf(p.p90), msf(c.p50), msf(p.p50 - c.p50)]);
      }));
      table(hk, ["records", "hook", "process per event, state written first", "process per event, now", "p90", "running client, now", "saved by a running client"], rows, [2, 3, 4, 5, 6], { cls: "wrap-head" });
    }
    const cs = $("#cold-table");
    if (cs) {
      table(cs, ["records", "first hook (starts the daemon)", "open the embedder (mapped)", "reading the table, as before"], L.map((r) => [int(r.records), msf(r.daemon_start_ms), msf(r.embedder_open_ms), msf(r.table_read_ms)]), [1, 2, 3]);
    }
  }

  const AB_SETS = [
    ["stockroom", "held out: stock keeping"],
    ["slotwise", "held out: appointments"],
    ["tally_dev", "Tally, Engram now"],
    ["tally_r1", "Tally, first Engram"],
  ];
  const AB_KIND = { conversation: "said only in conversation", repo: "in the repository", control: "control" };

  function abTable() {
    const el = $("#ab-table");
    const A = D.ab;
    if (!el || !A || !A.sets) return;
    const TN = { csv_export: "build the accountant's CSV export", exchange_rates: "fetch exchange rates", faster_save: "make saving faster", advice: "SQLite and pandas?", weighted_split: "split by weights", rename_api: "rename a public method", discount: "apply a discount", summary: "balance report", run_tests: "run the tests", slugify: "slugify helper" };
    const name = (k) => TN[k] || k.replace(/_/g, " ");
    const gate = (g) => { const c = {}; (g || []).forEach((d) => { c[d] = (c[d] || 0) + 1; }); return Object.entries(c).map(([k, v]) => `${k === "Skip" ? "none" : k === "Brief" ? "brief" : "full"} ×${v}`).join(", "); };
    const draw = (key) => {
      const S = A.sets[key];
      const kinds = ["conversation", "repo", "control"];
      const order = S.order.slice().sort((a, b) => kinds.indexOf(S.tasks[a].kind) - kinds.indexOf(S.tasks[b].kind));
      const rows = order.map((k) => {
        const t = S.tasks[k], e = t.engram, n = t.none;
        const cls = e.score > n.score + 1e-9 ? "sys" : "";
        const miss = (t.failed_checks.none || []).filter((c) => !(t.failed_checks.engram || []).includes(c));
        return [{ v: name(k), cls }, AB_KIND[t.kind] || t.kind || "", `${e.solved} of ${e.n}`, `${n.solved} of ${n.n}`, num(e.score, 2), num(n.score, 2), miss.join(", ") || "—", gate(e.gate), "$" + num(e.cost, 4), "$" + num(n.cost, 4), msf(e.steps), msf(n.steps), t.needs];
      });
      const o = S.overall;
      rows.push([{ v: "all", cls: "sys" }, "", `${o.engram.solved} of ${o.engram.n}`, `${o.none.solved} of ${o.none.n}`, num(o.engram.score, 2), num(o.none.score, 2), "", "", "$" + num(o.engram.cost, 4), "$" + num(o.none.cost, 4), msf(o.engram.steps), msf(o.none.steps), ""]);
      table(el, ["follow-up task", "the fact is", "solved, Engram", "solved, without", "score, Engram", "score, without", "checks failed only without Engram", "recall gate (Engram)", "cost, Engram (median)", "cost, without", "steps, Engram", "steps, without", "what it needs from earlier sessions"], rows, [2, 3, 4, 5, 8, 9, 10, 11], { cls: "wrap-head compact" });
    };
    tabs(el, AB_SETS.filter(([k]) => A.sets[k]), draw);
  }

  function abStats() {
    const el = $("#ab-stats");
    const st = D.ab && D.ab.stats;
    if (!el || !st) return;
    const LABEL = { "tally-r1": "Tally, first Engram", "tally-dev": "Tally, Engram now", stockroom: "held out: stock keeping", slotwise: "held out: appointments", pooled: "held out, both" };
    const p = (v) => (v == null ? "—" : v < 0.001 ? "< 0.001" : v.toFixed(3));
    const rows = [];
    ["pooled", "stockroom", "slotwise", "tally-dev", "tally-r1"].forEach((g) => {
      if (!st[g]) return;
      ["all", "conversation", "repo", "control"].forEach((k) => {
        const a = st[g][k];
        if (!a) return;
        const cls = k === "all" ? "sys" : "";
        rows.push([{ v: k === "all" ? LABEL[g] : "", cls }, { v: k === "all" ? "all tasks" : AB_KIND[k], cls }, `${a.solved.engram} of ${a.runs.engram}`, `${a.solved.none} of ${a.runs.none}`,
          num(a.score.engram, 2), num(a.score.none, 2), (a.perm_score.diff >= 0 ? "+" : "") + num(a.perm_score.diff, 2), `${num(a.score_diff_ci95[0], 2)} to ${num(a.score_diff_ci95[1], 2)}`,
          p(a.perm_solved.two_sided), p(a.perm_score.two_sided), p(a.fisher_solved.two_sided)]);
      });
    });
    table(el, ["runs", "tasks", "solved, Engram", "solved, without", "score, Engram", "score, without", "Δ score", "95% interval (tasks resampled)", "p, solved (permutation within tasks)", "p, score (permutation within tasks)", "p, solved (Fisher)"], rows, [2, 3, 4, 5, 6, 7, 8, 9, 10], { cls: "wrap-head compact" });
  }

  function abDistill() {
    const el = $("#ab-distill");
    const A = D.ab;
    if (!el || !A || !A.distill) return;
    const LABEL = { tally: "Tally (dev)", stockroom: "stock keeping (held out)", slotwise: "appointments (held out)" };
    const rows = Object.entries(A.distill).map(([k, d]) => {
      const h = (A.history || {})[k] || {};
      return [{ v: LABEL[k] || k, cls: "sys" }, d.mode, int(d.turns), int(d.kept), int(d.replaced), int(d.done), "$" + num(d.cost_usd, 4), "$" + num(d.cost_usd / d.turns, 4), num(d.ms_median / 1000, 1) + " s", num(d.ms_max / 1000, 1) + " s", h.cost != null ? "$" + num(h.cost, 3) : "—"];
    });
    table(el, ["history", "distilled", "turns", "items written", "items replaced", "planned items closed", "distillation cost", "per turn", "median time", "slowest", "the history's own cost"], rows, [2, 3, 4, 5, 6, 7, 8, 9, 10], { cls: "wrap-head compact" });
  }

  function meta() {
    const el = $("#meta-line");
    if (el) el.textContent = `Generated ${D.generated} · static embedder (Model2Vec potion-retrieval-32M) · results in engram/results/structure, reader, latency`;
  }

  meta();
  budgetChart();
  layoutTable();
  presTable();
  accuracyChart();
  typeTable();
  longTable();
  factsTable();
  productTable();
  gateTable();
  condTable();
  probesTable();
  latencyTables();
  abTable();
  abStats();
  abDistill();
  let last = window.innerWidth;
  window.addEventListener("resize", () => {
    if (Math.abs(window.innerWidth - last) < 40) return;
    last = window.innerWidth;
    document.querySelectorAll(".tabs").forEach((t) => t.remove());
    budgetChart();
    accuracyChart();
  });
})();
