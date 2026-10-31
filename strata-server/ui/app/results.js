// A query's answer, shown four ways: a grid (sort, resize, a row's
// details, copy and download), a chart of chosen columns, the plan as a
// tree of operators with what ran on each segment, and the raw JSON.

import { h, s, icon, clear, fmt, copy, drawer, showTip, hideTip, seg, empty } from "./core.js";
import { SERIES } from "./charts.js";

const isNum = (v) => typeof v === "number" && Number.isFinite(v);
const isTime = (v) => typeof v === "string" && /^\d{4}-\d\d-\d\dT\d\d:\d\d/.test(v);

/** A cell's value as text. */
export function cellText(v) {
  if (v == null) return "null";
  if (Array.isArray(v)) return v.length > 8 && v.every(isNum) ? `[${v.length} numbers]` : JSON.stringify(v);
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

function cell(v) {
  if (v == null) return h("span", { class: "nul" }, "null");
  if (typeof v === "boolean") return h("span", { class: "bool " + v }, String(v));
  if (isNum(v)) return h("span", { class: "n" }, Number.isInteger(v) ? fmt.n(v) : fmt.n(v, 4));
  if (Array.isArray(v)) {
    if (v.length > 8 && v.every(isNum)) return h("span", { class: "vec", title: `${v.length} numbers` }, `⟨${v.slice(0, 3).map((x) => x.toFixed(3)).join(", ")}, … ${v.length}⟩`);
    return h("span", { class: "chips" }, v.slice(0, 6).map((x) => h("span", { class: "badge" }, cellText(x))), v.length > 6 ? h("span", { class: "muted" }, `+${v.length - 6}`) : null);
  }
  if (typeof v === "object") return h("span", { class: "obj mono" }, JSON.stringify(v));
  return h("span", { title: v.length > 60 ? v : null }, v);
}

/** The rows of an answer as objects. */
export function rowsOf(j) { return (j.rows || []).map((r) => Object.fromEntries((j.columns || []).map((c, i) => [c, r[i]]))); }

function download(name, text, type) {
  const a = h("a", { href: URL.createObjectURL(new Blob([text], { type })), download: name });
  document.body.append(a); a.click(); a.remove();
}
const csvCell = (v, sep) => { const t = v == null ? "" : typeof v === "object" ? JSON.stringify(v) : String(v); return t.includes(sep) || t.includes('"') || t.includes("\n") ? `"${t.replace(/"/g, '""')}"` : t; };
export function asDelimited(j, sep = ",") { return [j.columns.map((c) => csvCell(c, sep)).join(sep), ...j.rows.map((r) => r.map((v) => csvCell(v, sep)).join(sep))].join("\n") + "\n"; }

/**
 * The grid of an answer: grid(j, {onRow}). Sorted by a column's header,
 * columns resized by their edge, a row's details on a click.
 */
export function grid(j, { max = 1000, onRow } = {}) {
  const cols = j.columns || [];
  const scored = (j.scores || []).some((x) => x != null && x !== 0);
  const order = (j.rows || []).map((_, i) => i);
  let sortCol = null, desc = false, shown = Math.min(order.length, 200);
  const widths = {};
  const thead = h("thead"), tbody = h("tbody");
  const tbl = h("table", { class: "t rgrid" }, thead, tbody);
  const more = h("div", { class: "row", style: { padding: "8px 10px" } });
  const wrap = h("div", { class: "grid-wrap" }, tbl, more);
  const maxScore = Math.max(1e-12, ...(j.scores || []).filter(isNum).map(Math.abs));

  function head() {
    clear(thead, h("tr", {}, h("th", { class: "rn" }, "#"), scored ? h("th", { class: "num", title: "the ORDER BY score" }, "score") : null,
      cols.map((c, ci) => {
        const th = h("th", { class: "sort", style: widths[ci] ? { width: widths[ci] + "px", maxWidth: widths[ci] + "px" } : null, title: "Sort by " + c }, h("span", {}, c), sortCol === ci ? h("span", { class: "arrow" }, desc ? "▼" : "▲") : null);
        const grip = h("span", { class: "rs", title: "Drag to resize" });
        grip.onpointerdown = (e) => {
          e.preventDefault(); e.stopPropagation();
          const x0 = e.clientX, w0 = th.getBoundingClientRect().width;
          const mv = (ev) => { widths[ci] = Math.max(48, w0 + ev.clientX - x0); th.style.width = th.style.maxWidth = widths[ci] + "px"; for (const tr of tbody.children) { const td = tr.children[ci + 1 + (scored ? 1 : 0)]; if (td) td.style.maxWidth = widths[ci] + "px"; } };
          const up = () => { removeEventListener("pointermove", mv); removeEventListener("pointerup", up); };
          addEventListener("pointermove", mv); addEventListener("pointerup", up);
        };
        th.append(grip);
        th.onclick = (e) => { if (e.target === grip) return; if (sortCol === ci) desc = !desc; else { sortCol = ci; desc = false; } sort(); head(); body(); };
        return th;
      })));
  }
  function sort() {
    order.sort((a, b) => {
      const x = j.rows[a][sortCol], y = j.rows[b][sortCol];
      const r = x == null ? 1 : y == null ? -1 : isNum(x) && isNum(y) ? x - y : cellText(x).localeCompare(cellText(y));
      return desc ? -r : r;
    });
  }
  function body() {
    clear(tbody, order.slice(0, shown).map((i, n) => {
      const r = j.rows[i];
      const tr = h("tr", { class: "click", tabIndex: 0 }, h("td", { class: "rn" }, n + 1),
        scored ? h("td", { class: "num sc" }, h("div", { class: "scbar", style: { width: Math.abs(j.scores[i] ?? 0) / maxScore * 100 + "%" } }), h("span", {}, j.scores[i] == null ? "–" : fmt.n(j.scores[i], 4))) : null,
        r.map((v, ci) => h("td", { class: isNum(v) ? "num" : "", style: widths[ci] ? { maxWidth: widths[ci] + "px" } : null }, cell(v))));
      const open = () => (onRow || rowDrawer)(i, j);
      tr.onclick = open; tr.onkeydown = (e) => { if (e.key === "Enter") open(); };
      tr.ondblclick = (e) => { const td = e.target.closest("td"); const ci = [...tr.children].indexOf(td) - 1 - (scored ? 1 : 0); if (ci >= 0) { e.stopPropagation(); copy(cellText(r[ci]), "Value copied"); } };
      return tr;
    }));
    clear(more, shown < order.length ? [h("span", { class: "small muted" }, `${fmt.n(shown)} of ${fmt.n(order.length)} rows shown`), h("button", { class: "btn sm", onclick: () => { shown = Math.min(order.length, shown + 500, max); body(); } }, "Show more")] : null);
    if (!order.length) clear(tbody, h("tr", {}, h("td", { colspan: cols.length + 2 }, empty("table", "No rows", "The query ran and matched nothing."))));
  }
  head(); body();
  return wrap;
}

/** One row of an answer, every column in full. */
export function rowDrawer(i, j) {
  const r = rowsOf(j)[i];
  drawer({
    title: `Row ${i + 1}`, sub: j.keys?.[i] != null ? `key ${cellText(j.keys[i])}` : null,
    body: [
      h("dl", { class: "kv" }, Object.entries(r).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, typeof v === "string" && v.length > 80 ? h("div", { class: "small", style: { whiteSpace: "pre-wrap" } }, v) : cell(v))])),
      j.scores?.[i] != null ? h("div", { class: "small muted" }, `score ${j.scores[i]}`) : null,
      h("pre", { class: "code", style: { maxHeight: "40vh" } }, JSON.stringify(r, null, 2)),
    ],
    foot: [h("button", { class: "btn", onclick: () => copy(JSON.stringify(r), "Row copied as JSON") }, icon("copy"), "Copy as JSON")],
  });
}

/** The tools over a grid: copy and download in four forms. */
export function exportMenu(j, name = "strata") {
  const stamp = new Date().toISOString().slice(0, 19).replace(/:/g, "");
  return [
    h("button", { class: "btn sm ghost", title: "Copy as tab-separated text, for a spreadsheet", onclick: () => copy(asDelimited(j, "\t"), `${fmt.plural(j.rows.length, "row")} copied`) }, icon("copy"), "Copy"),
    h("button", { class: "btn sm ghost", title: "Download as CSV", onclick: () => download(`${name}-${stamp}.csv`, asDelimited(j), "text/csv") }, icon("download"), "CSV"),
    h("button", { class: "btn sm ghost", title: "Download as JSON Lines, a row an object", onclick: () => download(`${name}-${stamp}.jsonl`, rowsOf(j).map((r) => JSON.stringify(r)).join("\n") + "\n", "application/x-ndjson") }, "JSONL"),
  ];
}

// ---------------------------------------------------------------- chart
/**
 * A chart of an answer's columns: a category or time down one axis and up
 * to four numeric columns, as bars, lines or dots; the columns are picked
 * from the answer's own and the first fitting ones start chosen.
 */
export function resultChart(j) {
  const cols = j.columns || [], rows = j.rows || [];
  const numeric = cols.filter((_, ci) => rows.some((r) => isNum(r[ci])) && rows.every((r) => r[ci] == null || isNum(r[ci])));
  if (!numeric.length || !rows.length) return empty("perf", "Nothing to chart", "A chart needs a numeric column: a COUNT(*), a SUM, a price…");
  const first = cols.find((c) => !numeric.includes(c)) ?? cols[0];
  let x = first, ys = numeric.filter((c) => c !== x).slice(0, 1), kind = rows.some((r) => isTime(r[cols.indexOf(x)])) ? "line" : "bar";
  if (!ys.length) ys = [numeric[0]];
  const xSel = h("select", { class: "select sm", onchange: () => { x = xSel.value; draw(); } }, cols.map((c) => h("option", { value: c, selected: c === x }, c)));
  const yBox = h("div", { class: "row", style: { gap: "4px" } });
  const kindSeg = seg([["bar", "Bars"], ["line", "Lines"], ["dot", "Dots"]], kind, (v) => { kind = v; draw(); });
  const plot = h("div", { class: "rchart" });
  const legend = h("div", { class: "legend" });
  function drawY() {
    clear(yBox, numeric.map((c) => { const on = ys.includes(c); return h("button", { class: "chip", "aria-pressed": String(on), onclick: () => { ys = on ? ys.filter((y) => y !== c) : [...ys, c].slice(-4); if (!ys.length) ys = [c]; draw(); } }, on ? h("span", { class: "dot", style: { background: SERIES[ys.indexOf(c)] } }) : null, c); }));
  }
  function draw() {
    drawY();
    const xi = cols.indexOf(x), yi = ys.map((y) => cols.indexOf(y));
    const pts = rows.slice(0, 400).map((r) => ({ x: r[xi], ys: yi.map((i) => r[i]) }));
    clear(legend, ys.length > 1 ? ys.map((y, k) => h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "k box", style: { background: SERIES[k], display: "inline-block" } }), y)) : null);
    clear(plot, svgChart(pts, ys, kind, x));
  }
  draw();
  return h("div", {}, h("div", { class: "row", style: { marginBottom: "8px", gap: "10px" } }, h("span", { class: "small muted" }, "across"), xSel, h("span", { class: "small muted" }, "show"), yBox, h("span", { class: "sep" }), kindSeg), legend, plot);
}

function svgChart(pts, names, kind, xName) {
  const W = 1000, H = 300, M = { l: 56, r: 14, t: 12, b: 46 };
  const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, style: { width: "100%", height: H + "px" }, role: "img", "aria-label": `${names.join(", ")} by ${xName}` });
  const all = pts.flatMap((p) => p.ys).filter(isNum);
  const lo = Math.min(0, ...all), hi = Math.max(...all, lo + 1e-9);
  const n = pts.length, iw = W - M.l - M.r, ih = H - M.t - M.b;
  const numX = pts.every((p) => isNum(p.x)) && kind !== "bar";
  const xs = numX ? pts.map((p) => p.x) : null;
  const x0 = numX ? Math.min(...xs) : 0, x1 = numX ? Math.max(...xs, x0 + 1e-9) : 1;
  const X = (i) => numX ? M.l + (pts[i].x - x0) / (x1 - x0) * iw : M.l + (i + 0.5) / n * iw;
  const Y = (v) => M.t + ih * (1 - (v - lo) / (hi - lo));
  for (let k = 0; k <= 4; k++) {
    const v = lo + (hi - lo) * k / 4;
    svgEl.append(s("line", { x1: M.l, x2: W - M.r, y1: Y(v), y2: Y(v), style: { stroke: "var(--grid)" } }), s("text", { x: M.l - 8, y: Y(v) + 3.5, "text-anchor": "end", style: { fill: "var(--muted)", fontSize: "11px" } }, fmt.si(v)));
  }
  svgEl.append(s("line", { x1: M.l, x2: W - M.r, y1: Y(0), y2: Y(0), style: { stroke: "var(--axis)" } }));
  const every = Math.max(1, Math.ceil(n / 14));
  pts.forEach((p, i) => { if (i % every === 0 && !numX) svgEl.append(s("text", { x: X(i), y: H - M.b + 16, "text-anchor": "end", transform: `rotate(-30 ${X(i)} ${H - M.b + 16})`, style: { fill: "var(--muted)", fontSize: "11px" } }, cellText(p.x).slice(0, 18))); });
  if (numX) for (let k = 0; k <= 4; k++) { const v = x0 + (x1 - x0) * k / 4; svgEl.append(s("text", { x: M.l + iw * k / 4, y: H - M.b + 16, "text-anchor": "middle", style: { fill: "var(--muted)", fontSize: "11px" } }, fmt.si(v))); }
  const group = iw / n, bw = Math.max(2, Math.min(36, (group - 4) / names.length - 2));
  names.forEach((name, k) => {
    const color = SERIES[k];
    if (kind === "bar") {
      pts.forEach((p, i) => {
        const v = p.ys[k]; if (!isNum(v)) return;
        const x = X(i) - (names.length * (bw + 2)) / 2 + k * (bw + 2) + 1;
        const y = Math.min(Y(v), Y(0)), hh = Math.max(1, Math.abs(Y(v) - Y(0)));
        const r = Math.min(4, bw / 2, hh);
        const d = v >= 0 ? `M${x},${y + hh}V${y + r}Q${x},${y} ${x + r},${y}H${x + bw - r}Q${x + bw},${y} ${x + bw},${y + r}V${y + hh}Z` : `M${x},${y}V${y + hh - r}Q${x},${y + hh} ${x + r},${y + hh}H${x + bw - r}Q${x + bw},${y + hh} ${x + bw},${y + hh - r}V${y}Z`;
        const bar = s("path", { d, style: { fill: color } });
        const hit = s("rect", { x: x - 1, y: M.t, width: bw + 2, height: ih, style: { fill: "transparent" } });
        for (const e of [bar, hit]) { e.addEventListener("pointermove", (ev) => { bar.style.opacity = 0.8; tip(ev, p); }); e.addEventListener("pointerleave", () => { bar.style.opacity = 1; hideTip(); }); }
        svgEl.append(bar, hit);
      });
    } else {
      const ok = pts.map((p, i) => [i, p.ys[k]]).filter(([, v]) => isNum(v));
      const order = numX ? ok.slice().sort((a, b) => pts[a[0]].x - pts[b[0]].x) : ok;
      if (kind === "line" && order.length > 1) svgEl.append(s("path", { d: order.map(([i, v], m) => `${m ? "L" : "M"}${X(i).toFixed(1)},${Y(v).toFixed(1)}`).join(""), class: "", style: { fill: "none", stroke: color, strokeWidth: 2, strokeLinejoin: "round" } }));
      for (const [i, v] of ok) {
        const c = s("circle", { cx: X(i), cy: Y(v), r: kind === "dot" ? 4.5 : 3, style: { fill: color, stroke: "var(--surface)", strokeWidth: 1.5 } });
        const hit = s("circle", { cx: X(i), cy: Y(v), r: 10, style: { fill: "transparent" } });
        hit.addEventListener("pointermove", (ev) => tip(ev, pts[i])); hit.addEventListener("pointerleave", hideTip);
        svgEl.append(c, hit);
      }
    }
  });
  function tip(ev, p) { showTip(ev.clientX, ev.clientY, [h("div", { class: "t" }, `${xName}: ${cellText(p.x)}`), ...names.map((nm, k) => h("div", { class: "r" }, h("span", { class: "k box", style: { background: SERIES[k] } }), h("b", {}, isNum(p.ys[k]) ? fmt.n(p.ys[k], 3) : "–"), h("span", { class: "n" }, nm)))]); }
  return svgEl;
}

// ---------------------------------------------------------------- plans
const OPS = ["Left join", "Distinct on", "Key order of", "Project", "Limit", "Sort", "Filter", "Aggregate", "Search", "Join", "With", "Ref", "Distinct", "Insert", "Update", "Delete", "Values", "Union", "Rerank", "Graph"];
const OP_COLOR = { Search: "var(--s1)", Join: "var(--s7)", "Left join": "var(--s7)", Aggregate: "var(--s4)", Sort: "var(--s3)", Limit: "var(--s3)", Filter: "var(--s2)", Insert: "var(--s8)", Update: "var(--s8)", Delete: "var(--s8)", With: "var(--s5)", Ref: "var(--s5)" };
const OP_WHAT = { Search: "reads one table: the rows its filter keeps, ranked by its order", Project: "names the columns", Limit: "keeps the first rows", Sort: "orders the rows", Filter: "keeps the rows (or groups) the condition holds for", Aggregate: "groups the rows and computes aggregates", Join: "pairs rows of its two inputs on a key", "Left join": "pairs rows, keeping the left ones without a match", With: "names a result that Ref reads", Ref: "reads a WITH result", Distinct: "keeps each row once", "Distinct on": "keeps the first row of each group", Insert: "writes rows", Update: "changes rows", Delete: "removes rows" };

/** EXPLAIN's lines as a tree: [{op, detail, props, kids}] and notes. */
export function parsePlan(lines) {
  const roots = [], notes = [], stack = [];
  for (const raw of lines || []) {
    const depth = Math.floor((raw.length - raw.trimStart().length) / 2), text = raw.trim();
    if (!text) continue;
    if (/^[a-z]/.test(text)) {
      const parent = [...stack].reverse().find((n) => n.depth < depth);
      if (parent) parent.props.push(text); else notes.push(text);
      continue;
    }
    const op = OPS.find((o) => text === o || text.startsWith(o + " ") || text.startsWith(o + ":")) || text.split(/[\s:]/)[0];
    const node = { op, detail: text.slice(op.length).replace(/^:\s*/, "").trim(), props: [], kids: [], depth };
    while (stack.length && stack[stack.length - 1].depth >= depth) stack.pop();
    (stack.length ? stack[stack.length - 1].kids : roots).push(node);
    stack.push(node);
  }
  return { roots, notes };
}

/** The plan as a tree of operator cards, the root on top and the reads at the bottom. */
export function planTree(lines) {
  const { roots, notes } = parsePlan(lines);
  const node = (n) => h("li", {},
    h("div", { class: "pn", style: { "--c": OP_COLOR[n.op] || "var(--muted)" }, title: OP_WHAT[n.op] || "" },
      h("div", { class: "op" }, n.op, n.op === "Search" ? icon("db") : null),
      n.detail ? h("div", { class: "dt mono" }, n.detail) : null,
      n.props.map((p) => { const [k, ...v] = p.split(": "); return h("div", { class: "pp" }, v.length ? [h("span", { class: "muted" }, k + " "), h("span", { class: "mono" }, v.join(": "))] : h("span", { class: "muted" }, p)); })),
    n.kids.length ? h("ul", {}, n.kids.map(node)) : null);
  return h("div", { class: "ptree-wrap" },
    h("ul", { class: "ptree" }, roots.map(node)),
    notes.length ? h("div", { class: "stack", style: { gap: "4px", marginTop: "10px" } }, notes.map((x) => h("div", { class: "small muted" }, icon("info"), " ", x))) : null);
}

/**
 * What the executor did, segment by segment: a tile per segment with the
 * strategies it chose, and the lines about the whole query above them.
 */
export function segments(lines) {
  const segs = new Map(), other = [];
  for (const l of lines || []) {
    const m = /^(?:shard (\d+): )?seg (\d+): (.*)$/.exec(l);
    if (m) { const k = (m[1] != null ? `shard ${m[1]} · ` : "") + "seg " + m[2]; if (!segs.has(k)) segs.set(k, []); segs.get(k).push(m[3]); }
    else other.push(l);
  }
  if (!segs.size && !other.length) return h("div", { class: "small muted" }, "The executor recorded nothing for this query.");
  return h("div", {},
    other.length ? h("div", { class: "stack", style: { gap: "3px", marginBottom: "10px" } }, other.map((l) => h("div", { class: "small mono dim" }, l))) : null,
    segs.size ? h("div", { class: "segs" }, [...segs].map(([k, ls]) => {
      const zero = ls.some((x) => /\b0 docs\b/.test(x));
      return h("div", { class: "sg" + (zero ? " skip" : ""), title: ls.join("\n") }, h("div", { class: "k" }, k, zero ? h("span", { class: "badge" }, "skipped") : null), ls.map((x) => h("div", { class: "small" }, x.split(/, /).map((p, i) => [i ? " · " : "", h("span", { class: /maxscore|bmw|wand|hnsw|ivf|flat|exhaustive|materiali|probe|scan|post|pre/i.test(p) ? "strat" : "" }, p)]))));
    })) : null);
}

/** An answer's figures: rows, time, rows that passed the filter, scored. */
export function answerStats(j) {
  const st = j.stats || j;
  return h("div", { class: "row small", style: { gap: "14px" } },
    j.written != null ? h("span", {}, h("b", {}, fmt.n(j.written)), " written") : h("span", {}, h("b", {}, fmt.plural(j.rows?.length ?? j.rows ?? 0, "row"))),
    h("span", {}, h("b", {}, fmt.ms(j.took_ms)), " on the server"),
    st.filter_docs != null ? h("span", { title: "rows the WHERE clause kept, before ranking" }, h("b", {}, fmt.n(st.filter_docs)), " passed the filter") : null,
    st.scored ? h("span", { title: "rows the ranking scored" }, h("b", {}, fmt.n(st.scored)), " scored") : null,
    st.reranked ? h("span", {}, h("b", {}, fmt.n(st.reranked)), " reranked") : null,
    j.partial ? h("span", { class: "pill warn" }, icon("warn"), `missing shards ${j.partial.missing_shards.join(", ")}`) : null);
}
