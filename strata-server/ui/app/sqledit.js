// A SQL editor: a textarea over a highlighted copy of its text, with line
// numbers, the current line, completion of tables, their fields, functions,
// keywords and parameters, the signature of the function the caret is in,
// a mark where the server says an error is, and keys to run (Ctrl+Enter)
// and explain (Ctrl+Shift+Enter).

import { h, clear, highlightSql } from "./core.js";

/** Strata's functions: [name, arguments, what it is]. */
export const FUNCS = [
  ["MATCH", "field, 'words'", "a full-text condition: words, \"phrases\", prefix*, typo~, AND, OR, NOT"],
  ["bm25", "field, 'text'", "the BM25 score of a text field for the words"],
  ["similarity", "field, :vector", "cosine similarity to a vector, or a sparse field's weights"],
  ["distance", "field, :vector", "distance to a vector; ORDER BY it ASC"],
  ["maxsim", "field, :vectors", "late interaction: each query vector's best match, summed"],
  ["rrf", "score, score, …", "reciprocal rank fusion of rankings"],
  ["minmax", "score", "a score scaled to 0..1, to add in weighted sums"],
  ["ranked", ":keys", "a ranking given as a list of keys"],
  ["decay", "field, origin, scale", "closeness to a value or time: 1 at origin, half at scale"],
  ["score", "", "the ORDER BY score of each row"],
  ["hops", "", "a row's distance from the seeds of a graph walk"],
  ["seq_no", "", "the sequence number of the write that last changed the row"],
  ["features", "", "the values the query's ranking read"],
  ["REACHABLE", "edges, seeds, hops => 2", "the rows within some hops of the seeds"],
  ["PATH", "edges, from, to", "the rows of one shortest path"],
  ["ppr", "edges, seeds", "personalized PageRank from the seeds"],
  ["PREFIX", "keyword, 'p'", "the values that start with p"],
  ["sid_at", "sid, n", "the first n numbers of a semantic ID"],
  ["gbdt", "'model'", "a gradient-boosted trees model's score"],
  ["COUNT", "*", "the rows of each group"],
  ["SUM", "x", "the sum of each group"],
  ["AVG", "x", "the mean of each group"],
  ["MIN", "x", "the smallest of each group"],
  ["MAX", "x", "the largest of each group"],
  ["now", "", "the time of the statement"],
];
const FN = new Map(FUNCS.map((f) => [f[0].toLowerCase(), f]));
const KEYWORDS = ("SELECT FROM WHERE AND OR NOT IN IS NULL BETWEEN LIKE ORDER BY GROUP HAVING LIMIT OFFSET ASC DESC NULLS FIRST LAST DISTINCT ON AS JOIN LEFT INNER " +
  "WITH EXISTS CASE WHEN THEN ELSE END TRUE FALSE INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE DROP IF PRIMARY KEY REFERENCES DEFAULT OPTIONS EXPLAIN").split(" ");
const TABLE_AFTER = /\b(FROM|JOIN|INTO|UPDATE|TABLE|EXISTS)\s+[A-Za-z_"]*$/i;

/**
 * editor({value, onChange, onRun, onExplain, catalog, params}). `catalog()`
 * gives the tables ([{table, short: {fields}}]); `params()` the names of
 * the parameters. Returns {el, value, focus, insert, markError, clearMarks}.
 */
export function editor({ value = "", onChange, onRun, onExplain, catalog = () => [], params = () => [], label = "SQL" } = {}) {
  const ta = h("textarea", { class: "ed-ta", spellcheck: "false", autocapitalize: "off", autocomplete: "off", wrap: "off", "aria-label": label, "aria-autocomplete": "list" });
  ta.value = value;
  const hl = h("pre", { class: "ed-hl", "aria-hidden": "true" });
  const gut = h("div", { class: "ed-gut", "aria-hidden": "true" });
  const cur = h("div", { class: "ed-cur", "aria-hidden": "true" });
  const err = h("div", { class: "ed-err hidden", "aria-hidden": "true" });
  const pop = h("div", { class: "ed-pop hidden", role: "listbox" });
  const sig = h("div", { class: "ed-sig hidden", role: "status" });
  const probe = h("span", { class: "ed-probe", "aria-hidden": "true" }, "x".repeat(64));
  const body = h("div", { class: "ed-body" }, cur, hl, err, ta, probe);
  const el = h("div", { class: "ed" }, gut, body, pop, sig);

  let cw = 7.5, lh = 20, padL = 12, padT = 10;
  function measure() {
    const r = probe.getBoundingClientRect();
    if (r.width) cw = r.width / 64;
    const cs = getComputedStyle(ta);
    lh = parseFloat(cs.lineHeight) || 20; padL = parseFloat(cs.paddingLeft) || 12; padT = parseFloat(cs.paddingTop) || 10;
  }

  function paint() {
    clear(hl, highlightSql(ta.value), "\n");
    const n = ta.value.split("\n").length;
    if (gut.childElementCount !== n) clear(gut, Array.from({ length: n }, (_, i) => h("div", {}, i + 1)));
    sync();
  }
  function sync() {
    hl.scrollTop = ta.scrollTop; hl.scrollLeft = ta.scrollLeft; gut.scrollTop = ta.scrollTop;
    const line = lineOf(ta.selectionStart);
    cur.style.transform = `translateY(${padT + line * lh - ta.scrollTop}px)`;
    cur.style.height = lh + "px";
    for (const [i, d] of [...gut.children].entries()) d.classList.toggle("on", i === line);
    if (err.dataset.at) placeErr();
  }
  const lineOf = (pos) => { let n = 0; for (let i = 0; i < pos; i++) if (ta.value.charCodeAt(i) === 10) n++; return n; };
  function caretXY(pos) {
    const before = ta.value.slice(0, pos);
    const line = before.split("\n").length - 1;
    const col = pos - before.lastIndexOf("\n") - 1;
    const r = ta.getBoundingClientRect(), b = el.getBoundingClientRect();
    return { x: r.left - b.left + padL + col * cw - ta.scrollLeft, y: r.top - b.top + padT + (line + 1) * lh - ta.scrollTop, line, col };
  }

  // ---------------------------------------------------------- editing
  function insert(text, from = ta.selectionStart, to = ta.selectionEnd) {
    ta.focus();
    ta.setSelectionRange(from, to);
    // insertText keeps the browser's undo history.
    if (!document.execCommand?.("insertText", false, text)) { ta.setRangeText(text, from, to, "end"); ta.dispatchEvent(new Event("input")); }
  }
  function lines(fn) {
    const v = ta.value, s = v.lastIndexOf("\n", ta.selectionStart - 1) + 1;
    let e = v.indexOf("\n", ta.selectionEnd - (ta.selectionEnd > ta.selectionStart && v[ta.selectionEnd - 1] === "\n" ? 1 : 0));
    if (e < 0) e = v.length;
    const out = v.slice(s, e).split("\n").map(fn).join("\n");
    insert(out, s, e);
    ta.setSelectionRange(s, s + out.length);
  }

  ta.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    if (!pop.classList.contains("hidden")) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); move(e.key === "ArrowDown" ? 1 : -1); return; }
      if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); accept(); return; }
      if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); closePop(); return; }
    }
    if (mod && e.key === "Enter") { e.preventDefault(); closePop(); (e.shiftKey ? onExplain : onRun)?.(); return; }
    if (mod && (e.key === " " || e.code === "Space")) { e.preventDefault(); complete(true); return; }
    if (mod && e.key === "/") { e.preventDefault(); const all = ta.value.slice(ta.selectionStart, ta.selectionEnd) || ta.value.slice(ta.value.lastIndexOf("\n", ta.selectionStart - 1) + 1).split("\n")[0]; const off = all.split("\n").every((l) => /^\s*--/.test(l) || !l.trim()); lines((l) => off ? l.replace(/^(\s*)-- ?/, "$1") : l.trim() ? "-- " + l : l); return; }
    if (e.key === "Tab") {
      e.preventDefault();
      if (ta.selectionStart !== ta.selectionEnd && ta.value.slice(ta.selectionStart, ta.selectionEnd).includes("\n")) lines((l) => e.shiftKey ? l.replace(/^ {1,2}/, "") : "  " + l);
      else if (!e.shiftKey) insert("  ");
      return;
    }
    if (e.key === "Enter" && !mod && !e.shiftKey) {
      e.preventDefault();
      const v = ta.value, s = v.lastIndexOf("\n", ta.selectionStart - 1) + 1;
      const ind = /^\s*/.exec(v.slice(s, ta.selectionStart))[0];
      const open = /\(\s*$/.test(v.slice(s, ta.selectionStart));
      if (open && v[ta.selectionStart] === ")") { const p = ta.selectionStart; insert("\n" + ind + "  \n" + ind); ta.setSelectionRange(p + ind.length + 3, p + ind.length + 3); }
      else insert("\n" + ind + (open ? "  " : ""));
      return;
    }
    // Brackets and quotes close themselves; typing the closer steps over it.
    const next = ta.value[ta.selectionStart];
    if ((e.key === ")" || e.key === "'") && next === e.key && ta.selectionStart === ta.selectionEnd) { e.preventDefault(); ta.setSelectionRange(ta.selectionStart + 1, ta.selectionStart + 1); sync(); return; }
    if ((e.key === "(" || e.key === "'") && !mod && (next == null || /[\s),;]/.test(next))) {
      const prev = ta.value[ta.selectionStart - 1];
      if (e.key === "'" && prev && /\w/.test(prev)) return;
      e.preventDefault();
      const sel = ta.value.slice(ta.selectionStart, ta.selectionEnd), p = ta.selectionStart;
      insert(e.key + sel + (e.key === "(" ? ")" : "'"));
      ta.setSelectionRange(p + 1, p + 1 + sel.length);
      signature();
    }
  });
  ta.addEventListener("input", (e) => {
    paint();
    clearMarks();
    onChange?.(ta.value);
    const typed = e.data ?? "";
    if (/^[A-Za-z_.:]$/.test(typed)) complete(false); else if (!/^\w$/.test(typed)) closePop(); else complete(false);
    signature();
  });
  ta.addEventListener("scroll", () => { sync(); closePop(); });
  for (const ev of ["click", "keyup", "select"]) ta.addEventListener(ev, (e) => { if (e.type === "keyup" && /^Arrow|Home|End|Page/.test(e.key) && pop.classList.contains("hidden")) signature(); sync(); });
  ta.addEventListener("blur", () => setTimeout(() => { if (!el.contains(document.activeElement)) { closePop(); sig.classList.add("hidden"); } }, 120));

  // ---------------------------------------------------------- completion
  let items = [], sel = 0, from = 0;
  function tablesIn(text) {
    const out = new Map();
    for (const m of text.matchAll(/\b(?:FROM|JOIN|UPDATE|INTO)\s+("?)([A-Za-z_][\w]*)\1(?:\s+(?:AS\s+)?([A-Za-z_]\w*))?/gi)) {
      const t = catalog().find((x) => x.table.toLowerCase() === m[2].toLowerCase());
      if (!t) continue;
      out.set(t.table.toLowerCase(), t);
      const alias = m[3] && !/^(WHERE|JOIN|LEFT|INNER|ON|ORDER|GROUP|LIMIT|SET|VALUES|USING)$/i.test(m[3]) ? m[3] : null;
      if (alias) out.set(alias.toLowerCase(), t);
    }
    return out;
  }
  function complete(forced) {
    const pos = ta.selectionStart, v = ta.value, before = v.slice(0, pos);
    if ((before.match(/'/g) || []).length % 2 === 1 || /--[^\n]*$/.test(before)) { closePop(); return; }
    const m = /(:?)([A-Za-z_][\w]*)?$/.exec(before);
    const dot = /([A-Za-z_]\w*)\.([A-Za-z_]\w*)?$/.exec(before);
    const word = (dot ? dot[2] : m[2]) || "";
    if (!forced && !word && !dot && !m[1]) { closePop(); return; }
    from = pos - word.length - (m[1] && !dot ? 1 : 0);
    const q = word.toLowerCase();
    const scope = tablesIn(v);
    let cands = [];
    if (m[1] && !dot) cands = params().map((p) => ({ text: ":" + p, label: ":" + p, kind: "param", detail: "parameter" }));
    else if (dot) {
      const t = scope.get(dot[1].toLowerCase());
      if (t) cands = fieldsOf(t);
    } else if (TABLE_AFTER.test(before)) {
      cands = catalog().map((t) => ({ text: t.table, label: t.table, kind: "table", detail: `${t.short?.fields?.length || 0} fields` }));
    } else {
      const seen = new Set();
      for (const t of new Set(scope.values())) for (const f of fieldsOf(t)) if (!seen.has(f.text)) { seen.add(f.text); cands.push(f); }
      if (!scope.size) for (const t of catalog()) for (const f of fieldsOf(t)) if (!seen.has(f.text)) { seen.add(f.text); cands.push(f); }
      cands.push(...FUNCS.map(([n, a, d]) => ({ text: n, label: n, kind: "fn", detail: `${n}(${a})`, doc: d, fn: true })));
      cands.push(...KEYWORDS.map((k) => ({ text: k, label: k, kind: "kw", detail: "" })));
      cands.push(...catalog().map((t) => ({ text: t.table, label: t.table, kind: "table", detail: "table" })));
    }
    const rank = (c) => { const l = c.label.toLowerCase().replace(/^:/, ""); return l.startsWith(q) ? 0 : l.includes(q) ? 1 : 9; };
    items = cands.filter((c) => !q || rank(c) < 9).filter((c) => !(c.label.toLowerCase() === q && !forced)).sort((a, b) => rank(a) - rank(b) || kindOrder(a) - kindOrder(b) || a.label.localeCompare(b.label)).slice(0, 60);
    if (!items.length) { closePop(); return; }
    sel = 0;
    drawPop(q);
    const xy = caretXY(from);
    pop.style.left = Math.max(0, Math.min(xy.x, el.clientWidth - 340)) + "px";
    pop.style.top = (xy.y + 2) + "px";
    pop.classList.remove("hidden");
    ta.setAttribute("aria-expanded", "true");
  }
  const kindOrder = (c) => ({ field: 0, param: 0, table: 1, fn: 2, kw: 3 }[c.kind] ?? 4);
  function fieldsOf(t) {
    return (t.short?.fields || []).map((f) => ({ text: needsQuote(f.name) ? `"${f.name}"` : f.name, label: f.name, kind: "field", detail: typeName(f) + (t.short.key === f.name ? " · key" : ""), table: t.table }));
  }
  function drawPop(q) {
    clear(pop, items.map((it, i) => {
      const r = h("div", { class: "it" + (i === sel ? " on" : ""), role: "option", "aria-selected": String(i === sel) },
        h("span", { class: "k k-" + it.kind }, { field: "f", table: "t", fn: "ƒ", kw: "k", param: ":" }[it.kind]),
        h("span", { class: "l" }, mark(it.label, q)), h("span", { class: "d" }, it.detail));
      r.onmousedown = (e) => { e.preventDefault(); sel = i; accept(); };
      r.onmousemove = () => { if (sel !== i) { sel = i; [...pop.children].forEach((c, j) => c.classList.toggle("on", j === i)); } };
      return r;
    }));
    const on = items[sel];
    if (on?.doc) pop.append(h("div", { class: "doc" }, on.doc));
  }
  function move(d) { sel = (sel + d + items.length) % items.length; drawPop(ta.value.slice(from, ta.selectionStart).replace(/^:/, "").toLowerCase()); pop.querySelector(".on")?.scrollIntoView({ block: "nearest" }); }
  function accept() {
    const it = items[sel]; if (!it) return;
    closePop();
    const nextCh = ta.value[ta.selectionStart];
    const text = it.fn && nextCh !== "(" ? it.text + "()" : it.text;
    insert(text, from, ta.selectionStart);
    if (it.fn && nextCh !== "(") { const p = ta.selectionStart - 1; ta.setSelectionRange(p, p); }
    signature();
  }
  function closePop() { pop.classList.add("hidden"); ta.setAttribute("aria-expanded", "false"); }

  // The function whose parentheses the caret is in, and which argument.
  function signature() {
    const before = ta.value.slice(0, ta.selectionStart);
    let depth = 0, arg = 0;
    for (let i = before.length - 1; i >= 0; i--) {
      const c = before[i];
      if (c === ")") depth++;
      else if (c === "," && depth === 0) arg++;
      else if (c === "(") {
        if (depth) { depth--; continue; }
        const name = /([A-Za-z_]\w*)\s*$/.exec(before.slice(0, i))?.[1]?.toLowerCase();
        const f = name && FN.get(name);
        if (!f) break;
        const args = f[1] ? f[1].split(", ") : [];
        clear(sig, h("span", { class: "mono" }, h("b", {}, f[0]), "(", args.map((a, j) => [j ? ", " : "", j === Math.min(arg, args.length - 1) ? h("u", {}, a) : a]), ")"), h("span", { class: "muted" }, " · " + f[2]));
        const xy = caretXY(i);
        sig.style.left = Math.max(0, Math.min(xy.x, el.clientWidth - 420)) + "px";
        sig.style.top = Math.max(0, xy.y - lh - 30) + "px";
        sig.classList.remove("hidden");
        return;
      } else if (c === ";") break;
    }
    sig.classList.add("hidden");
  }

  // ---------------------------------------------------------- errors
  function placeErr() {
    const [line, col] = err.dataset.at.split(":").map(Number);
    const lineStart = ta.value.split("\n").slice(0, line).reduce((a, l) => a + l.length + 1, 0);
    const len = Math.max(2, (/^\w+/.exec(ta.value.slice(lineStart + col))?.[0].length || 2));
    err.style.left = (padL + col * cw - ta.scrollLeft) + "px";
    err.style.top = (padT + (line + 1) * lh - ta.scrollTop - 3) + "px";
    err.style.width = (len * cw) + "px";
  }
  /** Marks where an error is: (line, column) from 1, as the server says them. */
  function markError(line, col) { err.dataset.at = `${Math.max(0, line - 1)}:${Math.max(0, col - 1)}`; err.classList.remove("hidden"); placeErr(); }
  function clearMarks() { err.classList.add("hidden"); delete err.dataset.at; }

  requestAnimationFrame(() => { measure(); paint(); });
  new ResizeObserver(() => { measure(); sync(); }).observe(body);
  paint();
  return {
    el,
    get value() { return ta.value; },
    set value(v) { ta.value = v; paint(); clearMarks(); },
    focus() { ta.focus(); },
    /** The selection: [start, end]. */
    get selection() { return [ta.selectionStart, ta.selectionEnd]; },
    select(a, b) { ta.focus(); ta.setSelectionRange(a, b); sync(); },
    insert,
    markError,
    clearMarks,
    /** Marks the place an error message names ("Line: 2, Column: 9"). */
    markFrom(message) { const m = /Line: (\d+), Column: (\d+)/.exec(message || ""); if (m) markError(+m[1], +m[2]); return !!m; },
  };
}

/** A field's type as the schema's SQL writes it. */
export function typeName(f) {
  const base = { text: "TEXT", keyword: "KEYWORD", int: "INT", float: "FLOAT", bool: "BOOL", timestamp: "TIMESTAMP", sparse: "SPARSE", edges: "EDGES", media: "MEDIA" }[f.type] ?? (f.type === "vector" ? (f.dim ? `VECTOR(${f.dim})` : "VECTOR") : String(f.type).toUpperCase());
  return base + (f.multi ? "[]" : "");
}

/** Whether a name needs double quotes in SQL. */
export function needsQuote(n) {
  return !/^[A-Za-z_][A-Za-z0-9_]*$/.test(n) || /^(select|from|where|order|group|by|limit|table|and|or|not|in|is|null|join|on|as|having|with|case|when|then|else|end|insert|update|delete|values|set|create|drop|primary|key|references|default|distinct|union|all|offset|asc|desc|like|between|exists|true|false)$/i.test(n);
}
export const quoteName = (n) => needsQuote(n) ? `"${n.replace(/"/g, '""')}"` : n;

function mark(text, q) {
  const i = q ? text.toLowerCase().indexOf(q) : -1;
  return i < 0 ? text : [text.slice(0, i), h("b", {}, text.slice(i, i + q.length)), text.slice(i + q.length)];
}
