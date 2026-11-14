// The SQL workbench: tabs of queries in an editor that completes tables,
// fields and functions, parameters filled in a form (a vector parameter
// can be text the server embeds), the answer as a grid, a chart, the plan
// as a tree with what ran on each segment, its profile (every step it
// took, with the request's trace and CPU time when asked) and the JSON; a
// sidebar of tables and fields to drag into the editor, snippets written
// for the table's own fields, history and saved queries.
//
// Writes have guard rails (sqlguard.js): a mode bar names the cluster, its
// environment, its shards and the key; production is read-only until
// writes are allowed for 15 minutes with a reason; the statement that will
// run is marked and the Run button says its kind ("Run SELECT", "Check
// UPDATE"); and every write waits for a check of the rows it changes.

import { h, icon, clear, fmt, api, store, bus, toast, copy, modal, drawer, empty, seg, sqlBlock, debounce, ApiError, failure, tipOn, signInNeeded, hasToken } from "../core.js";
import { editor, quoteName, typeName } from "../sqledit.js";
import { tables, cached, invalidate, typeBadge } from "../catalog.js";
import { folded, firstMine } from "../owners.js";
import { grid, resultChart, planTree, segments, answerStats, exportMenu } from "../results.js";
import { tooBig, firstPage, banner as cappedBanner } from "../bigresult.js";
import { healthView } from "../health.js";
import { profileView } from "../profile.js";
import { state, isProduction, isSandbox } from "../data.js";
import { parseStatement, runLabel, mode, askAllow, checkWrite, endWrites, writesAllowed, whereName, reach, nearNames, replaceWord } from "../sqlguard.js";
import { followsWho, writeThere, followBanner } from "../follows.js";

export default { id: "sql", title: "SQL workbench", icon: "sql", group: "Data", full: true, range: false, keywords: "query editor sql run explain plan results chart history saved snippets parameters", render, onParams };

let openSql = null;
function onParams(r) { const q = r.params.get("q"); if (q && openSql) openSql(q, r.params.get("p"), r.params.get("store")); }

const uid = () => Math.random().toString(36).slice(2, 9);
const READ = /^\s*(\(|SELECT\b|WITH\b)/i;
const stripStrings = (sql) => sql.replace(/'(?:[^']|'')*'/g, "''").replace(/--[^\n]*/g, "");

/** The `:name` parameters a statement uses, in order. */
export function paramNames(sql) { return [...new Set([...stripStrings(sql).matchAll(/(?<![:\w]):([A-Za-z_]\w*)/g)].map((m) => m[1]))]; }

/**
 * Where each `;` that ends a statement is in `text`: not one in a string
 * ('it''s'), a quoted name ("a;b"), a `--` comment to the end of its line
 * or a block comment (an apostrophe there is no string: "-- the shop's").
 */
export function splitPoints(text) {
  const out = [];
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === "'" || c === '"') { const j = text.indexOf(c, i + 1); if (j < 0) break; i = j; }
    else if (c === "-" && text[i + 1] === "-") { const j = text.indexOf("\n", i); if (j < 0) break; i = j; }
    else if (c === "/" && text[i + 1] === "*") { const j = text.indexOf("*/", i + 2); if (j < 0) break; i = j + 1; }
    else if (c === ";") out.push(i);
  }
  return out;
}
/** The statements of `text` that hold more than comments and spaces: [{sql, start}]. */
export function statements(text) {
  const out = [];
  let start = 0;
  for (const i of [...splitPoints(text), text.length]) {
    const sql = text.slice(start, i);
    if (stripStrings(sql).replace(/\/\*[\s\S]*?\*\//g, "").trim()) out.push({ sql, start, end: i });
    start = i + 1;
  }
  return out;
}

/** The statement the caret is in, or the selection, and where in the text it starts. */
export function statementAt(text, [a, b]) {
  if (b > a) return { sql: text.slice(a, b), start: a };
  const all = statements(text);
  const at = all.find((s) => a <= s.end) || all[all.length - 1];
  return at ? { sql: at.sql, start: at.start } : { sql: "", start: 0 };
}

/** The (line, column) from 1 of offset `pos` in `text`. */
function lineCol(text, pos) {
  const before = text.slice(0, pos);
  return [before.split("\n").length, pos - before.lastIndexOf("\n")];
}

/**
 * Where in the statement an error is, as an offset into it: the line and
 * column the parser gives ("at line 2, column 9"), or else the first place
 * the statement names the word the error is about (a column, a table, a
 * function). `null` when the error names no place.
 */
export function errorSpot(msg, sql) {
  const lc = /\bline:? (\d+),? column:? (\d+)/i.exec(msg || "");
  if (lc) {
    const lines = sql.split("\n"), line = Math.min(+lc[1], lines.length) - 1;
    return { at: lines.slice(0, line).reduce((a, l) => a + l.length + 1, 0) + Math.max(0, +lc[2] - 1), word: null };
  }
  if (/at the end of the statement/i.test(msg)) return { at: sql.replace(/\s+$/, "").length, word: null };
  const param = /^unbound parameter :(\w+)/i.exec(msg || "");
  if (param) { const i = sql.search(new RegExp(`:${param[1]}\\b`)); return i < 0 ? null : { at: i, word: null }; }
  const word = wrongWord(msg) || subjectOf(msg);
  if (!word) return null;
  // Strings and comments blanked out, so offsets stay the statement's.
  const blank = sql.replace(/'(?:[^']|'')*'/g, (x) => "'" + " ".repeat(x.length - 2) + "'").replace(/--[^\n]*/g, (x) => " ".repeat(x.length));
  const m = new RegExp(`(?<![\\w.'])(?:\\w+\\.)?${word}(?![\\w'])`, "i").exec(blank);
  return m ? { at: m.index + m[0].length - word.length, word } : null;
}

/** The name an error is about: the misspelt column, table, function or keyword, or the field searched that cannot be. */
export function wrongWord(msg) {
  const m = /no such (?:column|field|table)s?:?\s+"?([\w.]+?)"?(?=[\s;(,]|$)/i.exec(msg || "")
    || /unknown (?:ranking )?function:? "?(\w+)/i.exec(msg || "")
    || /, found "?(\w+)"? \(did you mean/i.exec(msg || "")
    || /: (\w+) reads as a name here/i.exec(msg || "")
    || /^(?:MATCH|bm25)\((\w+), \.\.\.\): \w+ is (?:stored but )?not indexed/i.exec(msg || "")
    || /^"?(\w+)"?\s*(?:[<>=!]+|IN\b|NOT\b|LIKE\b|BETWEEN\b|IS\b)[^:]*? compares no column of /i.exec(msg || "");
  return m ? m[1].split(".").pop() : null;
}

/** The column an error is about, to mark it ("price is a FLOAT: ..."). */
function subjectOf(msg) {
  const m = /^(\w+) is an? (?:KEYWORD|INT|FLOAT|TEXT|BOOL|TIMESTAMP|VECTOR|JSON|SPARSE|EDGES|MEDIA)\b/.exec(msg || "")
    || /^(?:similarity|maxsim|MATCH|bm25)\((\w+), \.\.\.\)/i.exec(msg || "");
  return m ? m[1] : null;
}

/** The names an error's "(did you mean a, b or c?)" offers. */
export function suggested(msg) {
  const m = /\(did you mean ([^?]+)\?\)/i.exec(msg || "");
  return m ? m[1].split(/,\s*|\s+or\s+/).map((x) => x.trim()).filter(Boolean) : [];
}

/** What kind of error a message is, for the heading of the error panel. */
export function errorTitle(msg, status) {
  if (status === 0) return "The server could not be reached";
  if (/^SQL syntax error/i.test(msg)) return "Syntax error";
  if (/^no such table/i.test(msg)) return "Unknown table";
  if (/^no such (column|field)/i.test(msg) || / compares no column of /i.test(msg)) return "Unknown column";
  if (/^unknown (ranking )?function/i.test(msg)) return "Unknown function";
  if (/^unbound parameter|^:\w+ is not JSON/i.test(msg)) return "A parameter needs a value";
  if (/^duplicate primary key/i.test(msg)) return "A row with that key exists";
  if (/^(SHOW|DESCRIBE|TRUNCATE|CREATE (INDEX|VIEW)|BEGIN|COMMIT|ROLLBACK|EXPLAIN)\b|is not a statement (?:Strata|ChloeDB) runs|(?:Strata|ChloeDB) runs one statement/i.test(msg)) return "Not a statement ChloeDB runs";
  if (/so it has no words to search/i.test(msg)) return "Not a searchable field";
  if (/^\w+ is an? (?:KEYWORD|INT|FLOAT|TEXT|BOOL|TIMESTAMP|VECTOR|JSON)\b| takes a vector|^CAST\(/.test(msg)) return "A value of the wrong type";
  if (status === 401 || status === 403) return "Not allowed";
  if (status >= 500) return "The server failed";
  return "The statement was refused";
}

/**
 * A statement the workbench answers itself from the catalog, as other
 * databases' shells do: SHOW TABLES, DESCRIBE t (DESC t, SHOW COLUMNS FROM
 * t) and SHOW CREATE TABLE t. An answer like the server's, or null.
 */
export function localAnswer(sql, ts) {
  const q = sql.trim().replace(/;\s*$/, "").replace(/\s+/g, " ");
  const unquote = (n) => n.replace(/^"(.*)"$/, "$1").replace(/""/g, '"').replace(/^`(.*)`$/, "$1");
  const table = (n) => {
    const t = ts.find((x) => x.table === unquote(n)) || ts.find((x) => x.table.toLowerCase() === unquote(n).toLowerCase());
    if (!t) throw new Error(`no such table: ${unquote(n)}${didYouMean(unquote(n), ts.map((x) => x.table))}; the database's tables are ${ts.map((x) => x.table).join(", ") || "none yet"}`);
    return t;
  };
  const answer = (columns, rows) => ({ columns, rows, took_ms: 0, local: true });
  if (/^SHOW (FULL )?TABLES$/i.test(q)) {
    return answer(["table", "primary_key", "columns", "fields"], ts.map((t) => [t.table, t.short?.key ?? null, (t.short?.fields || []).length, (t.short?.fields || []).map((f) => f.name).join(", ")]));
  }
  let m = /^(?:DESCRIBE|DESC|SHOW (?:FULL )?(?:COLUMNS|FIELDS) (?:FROM|IN)) ("(?:[^"]|"")+"|`[^`]+`|[\w.]+)$/i.exec(q);
  if (m) {
    const t = table(m[1]);
    return answer(["column", "type", "key", "indexed", "stored", "notes"], (t.short?.fields || []).map((f) => [f.name, typeName(f), f.name === t.short?.key, f.indexed !== false, stored(f), fieldNote(f)]));
  }
  m = /^SHOW CREATE TABLE ("(?:[^"]|"")+"|`[^`]+`|[\w.]+)$/i.exec(q);
  if (m) { const t = table(m[1]); return answer(["table", "create_table"], [[t.table, t.sql || ""]]); }
  return null;
}
const stored = (f) => f.stored ?? !(f.from || f.embed);
function fieldNote(f) {
  const from = (xs) => xs.join(", ");
  if (f.embed) return `embedded by ${f.embed.model} from ${from(f.embed.from || [])}: similarity(${f.name}, :q) with text for :q searches it (the server embeds it)`;
  if (f.from) return `made from ${from(f.from)}: MATCH(${f.name}, 'words') and bm25 search it`;
  if (f.type === "text" && f.indexed === false) return "stored only: SELECT returns it and LIKE compares it, but MATCH and bm25 cannot search it";
  if (f.type === "text") return `MATCH(${f.name}, 'words') and bm25 search it`;
  if (f.multi) return "a list: a condition holds when one of its values matches";
  return "";
}
function didYouMean(word, names) {
  const near = names.map((n) => [n, levenshtein(n.toLowerCase(), word.toLowerCase())]).filter(([, d]) => d > 0 && d <= (word.length < 5 ? 1 : 2)).sort((a, b) => a[1] - b[1]);
  return near.length ? ` (did you mean ${near.map(([n]) => n).slice(0, 3).join(" or ")}?)` : "";
}

const UPPER = new Set("select from where and or not in is null between like order by group having limit offset asc desc nulls distinct on as join left inner with exists case when then else end true false insert into values update set delete create table drop primary references explain union all".split(" "));
const CLAUSE1 = new Set(["SELECT", "FROM", "WHERE", "HAVING", "LIMIT", "OFFSET", "JOIN", "UNION", "VALUES", "SET", "WITH", "UPDATE"]);
const CLAUSE2 = new Set(["GROUP BY", "ORDER BY", "LEFT JOIN", "INNER JOIN", "INSERT INTO", "DELETE FROM"]);
/** Lays a statement out a clause a line, keywords in capitals, strings as they are. */
export function formatSql(sql) {
  const words = sql.match(/--[^\n]*|'(?:[^']|'')*'|"(?:[^"]|"")*"|:[A-Za-z_]\w*|[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*|[(),;]|[^\sA-Za-z_(),;'"]+/g) || [];
  let out = "", depth = 0, prev = "", skip = false, cond = false;
  for (let i = 0; i < words.length; i++) {
    let t = words[i];
    if (t.startsWith("--")) { out += (out && !out.endsWith("\n") ? "\n" : "") + t + "\n"; prev = ""; continue; }
    if (/^[A-Za-z_]\w*$/.test(t) && UPPER.has(t.toLowerCase())) t = t.toUpperCase();
    const two = t + " " + String(words[i + 1] || "").toUpperCase();
    const clause = depth === 0 && !skip && (CLAUSE2.has(two) || CLAUSE1.has(t));
    skip = depth === 0 && CLAUSE2.has(two);
    if (clause) cond = t === "WHERE" || t === "HAVING";
    const andOr = depth === 0 && cond && (t === "AND" || t === "OR") && !/BETWEEN\s+\S+$/.test(out);
    if (clause && out.trim()) out = out.replace(/\s+$/, "") + "\n";
    else if (andOr) out = out.replace(/\s+$/, "") + "\n  ";
    else if (out && !/[\n(]$/.test(out) && t !== ")" && t !== "," && t !== ";" && !(t === "(" && /^[A-Za-z_]/.test(prev) && !UPPER.has(prev.toLowerCase()))) out += " ";
    out += t;
    if (t === "(") depth++;
    if (t === ")") depth = Math.max(0, depth - 1);
    if (t === ";") out += "\n";
    prev = t;
  }
  return out.trim() + "\n";
}

function levenshtein(a, b) {
  const d = Array.from({ length: a.length + 1 }, (_, i) => [i]);
  for (let j = 1; j <= b.length; j++) d[0][j] = j;
  for (let i = 1; i <= a.length; i++) for (let j = 1; j <= b.length; j++) d[i][j] = Math.min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1));
  return d[a.length][b.length];
}

/** Snippets written for one table's own fields. */
export function snippets(t) {
  const fs = t?.short?.fields || [], name = quoteName(t?.table || "t"), key = t?.short?.key;
  const of = (ty) => fs.filter((f) => f.type === ty && !f.multi);
  // The field MATCH and bm25 search: an indexed TEXT field, best one made
  // from others (it covers them); a TEXT field stored but not indexed has
  // no words to search.
  const texts = of("text").filter((f) => f.indexed !== false);
  const text = texts.find((f) => f.from) || texts[0], vec = fs.find((f) => f.type === "vector" && !f.multi), kw = fs.find((f) => f.type === "keyword" && f.name !== key), num = fs.find((f) => f.type === "int" || f.type === "float"), ts = of("timestamp")[0], edges = fs.find((f) => f.type === "edges");
  const cols = [key, ...fs.filter((f) => f.name !== key && (f.type === "text" || f.type === "keyword") && stored(f)).slice(0, 2).map((f) => f.name)].filter(Boolean).map(quoteName).join(", ") || "*";
  const out = [];
  out.push(["First rows", "Every column of the first rows", `SELECT * FROM ${name} LIMIT 20;`]);
  if (text) out.push(["Search words", "Rows with the words, best first by BM25", `SELECT ${cols}, score()\nFROM ${name}\nWHERE MATCH(${quoteName(text.name)}, 'your words')\nORDER BY bm25(${quoteName(text.name)}, 'your words') DESC\nLIMIT 10;`]);
  if (text) out.push(["Allow typos", "Words within two edits, ranked", `SELECT ${cols}, score()\nFROM ${name}\nORDER BY bm25(${quoteName(text.name)}, 'your wrods', fuzzy => 'auto') DESC\nLIMIT 10;`]);
  if (vec) out.push(["Similar to a text", "Nearest rows to a text the server embeds (:q)", `SELECT ${cols}, score()\nFROM ${name}\nORDER BY similarity(${quoteName(vec.name)}, :q) DESC\nLIMIT 10;`]);
  if (vec && text) out.push(["Hybrid", "30% words, 70% meaning", `SELECT ${cols}, score()\nFROM ${name}\nORDER BY 0.3 * tmm(bm25(${quoteName(text.name)}, :words)) + 0.7 * tmm(similarity(${quoteName(vec.name)}, :q)) DESC\nLIMIT 10;`]);
  if (vec && text) out.push(["Hybrid by rank", "Words and meaning fused by rank (RRF)", `SELECT ${cols}, score()\nFROM ${name}\nORDER BY rrf(bm25(${quoteName(text.name)}, :words), similarity(${quoteName(vec.name)}, :q)) DESC\nLIMIT 10;`]);
  if (kw) out.push(["Facet", `Rows by ${kw.name}, largest first`, `SELECT ${quoteName(kw.name)}, COUNT(*) AS n${num ? `, AVG(${quoteName(num.name)}) AS avg_${num.name}` : ""}\nFROM ${name}\nGROUP BY ${quoteName(kw.name)}\nORDER BY n DESC\nLIMIT 20;`]);
  if (num && text) out.push(["Boost by a value", `BM25 times ${num.name}`, `SELECT ${cols}, ${quoteName(num.name)}, score()\nFROM ${name}\nORDER BY bm25(${quoteName(text.name)}, 'your words') * (1 + 0.1 * coalesce(${quoteName(num.name)}, 0)) DESC\nLIMIT 10;`]);
  if (ts && text) out.push(["Fresh first", "BM25 halved for every 30 days of age", `SELECT ${cols}, ${quoteName(ts.name)}, score()\nFROM ${name}\nORDER BY bm25(${quoteName(text.name)}, 'your words') * decay(${quoteName(ts.name)}, now(), 2592000) DESC\nLIMIT 10;`]);
  if (edges) out.push(["Graph walk", "Rows within two hops of a row", `SELECT ${cols}, hops()\nFROM ${name}\nWHERE REACHABLE(${quoteName(edges.name)}, ('a key'), hops => 2)\nORDER BY hops()\nLIMIT 50;`]);
  if (edges) out.push(["PageRank", "Rows by personalized PageRank from a seed", `SELECT ${cols}, score()\nFROM ${name}\nORDER BY ppr(${quoteName(edges.name)}, ('a key')) DESC\nLIMIT 20;`]);
  if (key) out.push(["One row", "A row by its key", `SELECT * FROM ${name} WHERE ${quoteName(key)} = :key;`]);
  return out;
}

function render(page, r) {
  let tabs = store.get("sql.tabs", null);
  if (!tabs?.length) tabs = [{ id: uid(), name: "Query 1", sql: "", params: {} }];
  let active = store.get("sql.active", tabs[0].id);
  if (!tabs.some((t) => t.id === active)) active = tabs[0].id;
  // The profile (trace and CPU of every step) is off on production unless
  // ticked for this visit; elsewhere it keeps what was chosen.
  let traceTouched = false;
  let trace = isProduction() ? false : store.get("sql.trace", true);
  let last = null, lastKind = "results", running = null;
  const tab = () => tabs.find((t) => t.id === active);
  const save = debounce(() => { store.set("sql.tabs", tabs); store.set("sql.active", active); }, 300);

  // ------------------------------------------------------------ the layout
  const side = h("aside", { class: "wb-side" });
  const tabBar = h("div", { class: "qtabs", role: "tablist" });
  const paramsBox = h("div", { class: "params hidden" });
  const out = h("div", { class: "wb-out" });
  const outTabs = h("div", { class: "tabs", role: "tablist", style: { marginBottom: "0" } });
  const outBody = h("div", { class: "wb-res" });
  const status = h("div", { class: "wb-status" });
  const runBtn = h("button", { class: "btn primary", title: "Run the statement at the caret (Ctrl+Enter)", onclick: () => run() }, icon("play"), "Run");
  const traceBox = h("input", { type: "checkbox", checked: trace, onchange: (e) => { trace = e.target.checked; traceTouched = true; if (!isProduction()) store.set("sql.trace", trace); } });
  const traceSw = h("label", { class: "row nw small dim", title: "Ask the server for the request's trace and the CPU time of every step the query took (its Profile tab). Off by default on production: it costs CPU on every shard.", style: { gap: "6px", cursor: "pointer" } }, traceBox, "profile");
  const modeBar = h("div", { class: "dt-mode", role: "status" });
  const ed = editor({
    value: tab().sql,
    onChange: (v) => { tab().sql = v; save(); drawParams(); },
    onRun: () => run(),
    onExplain: () => run(true),
    catalog: cached,
    params: () => paramNames(ed.value),
  });
  const edWrap = h("div", { class: "wb-ed", style: { height: store.get("sql.edH", 260) + "px" } }, ed.el);
  // The statement Ctrl+Enter runs, marked when the editor holds several.
  const band = h("div", { class: "dt-band hidden", "aria-hidden": "true" });
  const ta = ed.el.querySelector("textarea");
  ed.el.querySelector(".ed-body")?.prepend(band);
  function current() {
    const at = statementAt(ed.value, ed.selection);
    const lead = at.sql.length - at.sql.trimStart().length;
    return { ...at, from: at.start + lead, to: at.start + at.sql.trimEnd().length, p: parseStatement(at.sql) };
  }
  const many = () => statements(ed.value).length > 1;
  function mark() {
    const c = current();
    explainBtn.disabled = onLogs();
    explainBtn.title = onLogs() ? "The log store answers SELECT only: no plan" : "Show the plan without the trace (Ctrl+Shift+Enter)";
    const label = runLabel(c.p);
    if (!running && onLogs() && c.p.write) {
      // The log store takes no writes: say so on the button.
      clear(runBtn, icon("lock"), "Read-only here");
      runBtn.title = "The log store is read-only from here";
      runBtn.classList.remove("dt-write");
    } else if (!running) {
      clear(runBtn, icon(c.p.write ? (mode().readOnly ? "lock" : "shield") : "play"), label);
      runBtn.title = c.p.write ? `${label}: see the rows it changes before it runs (Ctrl+Enter)` : `${label}: the statement at the caret (Ctrl+Enter)`;
      runBtn.classList.toggle("dt-write", !!c.p.write);
    }
    if (!ta || !c.sql.trim() || !many() || ed.selection[1] > ed.selection[0]) { band.classList.add("hidden"); return; }
    const cs = getComputedStyle(ta), lh = parseFloat(cs.lineHeight) || 20, pt = parseFloat(cs.paddingTop) || 10;
    const l0 = ed.value.slice(0, c.from).split("\n").length - 1, l1 = ed.value.slice(0, c.to).split("\n").length - 1;
    band.style.top = (pt + l0 * lh - ta.scrollTop) + "px";
    band.style.height = ((l1 - l0 + 1) * lh) + "px";
    band.classList.remove("hidden");
    band.dataset.kind = c.p.write ? "write" : "read";
  }
  const markSoon = debounce(mark, 60);
  if (ta) for (const ev of ["input", "click", "keyup", "select", "scroll", "focus"]) ta.addEventListener(ev, markSoon);
  const splitter = h("div", { class: "splitter", title: "Drag to resize", role: "separator", "aria-orientation": "horizontal" });
  splitter.onpointerdown = (e) => {
    e.preventDefault();
    const y0 = e.clientY, h0 = edWrap.offsetHeight;
    const mv = (ev) => { edWrap.style.height = Math.max(120, Math.min(innerHeight - 220, h0 + ev.clientY - y0)) + "px"; };
    const up = () => { removeEventListener("pointermove", mv); removeEventListener("pointerup", up); store.set("sql.edH", edWrap.offsetHeight); };
    addEventListener("pointermove", mv); addEventListener("pointerup", up);
  };
  const explainBtn = h("button", { class: "btn", title: "Show the plan without the trace (Ctrl+Shift+Enter)", onclick: () => run(true) }, icon("layers"), "Explain");
  const tools = h("div", { class: "wb-tools" }, runBtn,
    explainBtn,
    h("button", { class: "btn ghost", title: "Lay the SQL out a clause a line", onclick: () => { ed.value = formatSql(ed.value); tab().sql = ed.value; save(); ed.focus(); } }, icon("menu"), "Format"),
    traceSw,
    h("span", { class: "sep" }),
    h("span", { class: "small muted hide-sm" }, h("kbd", {}, "Ctrl"), " ", h("kbd", {}, "Enter"), " run · ", h("kbd", {}, "Ctrl"), " ", h("kbd", {}, "Space"), " complete"),
    h("button", { class: "btn ghost icon", title: "Copy a link to this query", onclick: () => copy(location.origin + "/#/sql?q=" + encodeURIComponent(ed.value), "Link copied") }, icon("link")),
    h("button", { class: "btn ghost icon", title: "Save this query", onclick: saveQuery }, icon("plus")),
    h("button", { class: "btn ghost", title: "Saved queries and history", onclick: library }, icon("clock"), "Library"),
    // NEW-data-tools-9: under 1,100 px the sidebar is a sheet this opens.
    h("button", { class: "btn ghost wb-tables-btn", title: "The tables, their fields and snippets", onclick: () => sideOpen(true) }, icon("table"), "Tables"));
  // A cluster that follows another refuses its clients' writes: say so
  // above the editor (known with an admin key; a refused write says it too).
  const followEl = h("div", { class: "wb-follow" });
  followBanner(followEl, "sql").catch(() => {});
  const wbEl = h("div", { class: "wb" }, side, h("div", { class: "wb-main" }, tabBar, modeBar, followEl, tools, edWrap, paramsBox, splitter, out));
  const sideOpen = (on) => wbEl.classList.toggle("side-open", on);
  // On a narrow screen a pick from the sheet closes it.
  side.addEventListener("click", (e) => { if (e.target.closest(".fi, .snip, .tt-h .btn") && matchMedia("(max-width: 1100px)").matches) sideOpen(false); });
  page.append(wbEl);
  out.append(h("div", { class: "row between wb-outhd" }, outTabs, status), outBody);

  // ------------------------------------------------------------ tabs
  function drawTabs() {
    clear(tabBar, tabs.map((t) => {
      const b = h("button", { role: "tab", class: "qt", "aria-selected": String(t.id === active), title: "Double-click to rename" },
        h("span", {}, t.name),
        tabs.length > 1 ? h("span", { class: "x", title: "Close", onclick: (e) => { e.stopPropagation(); close(t.id); } }, "×") : null);
      b.onclick = () => select(t.id);
      b.ondblclick = () => { const n = prompt("Name this tab", t.name); if (n?.trim()) { t.name = n.trim(); save(); drawTabs(); } };
      return b;
    }), h("button", { class: "qt add", title: "A new query", onclick: () => add("") }, icon("plus")));
  }
  function select(id) { active = id; ed.value = tab().sql; save(); drawTabs(); drawParams(); drawMode(); markSoon(); }
  function add(sql, name, where) { const t = { id: uid(), name: name || (where === "logs" ? "Log store" : `Query ${tabs.length + 1}`), sql, params: {}, ...(where === "logs" ? { store: "logs" } : {}) }; tabs.push(t); select(t.id); ed.focus(); return t; }
  function close(id) { const i = tabs.findIndex((t) => t.id === id); tabs.splice(i, 1); if (active === id) active = tabs[Math.max(0, i - 1)].id; select(active); }
  // `p` is the parameters as JSON: the query's own ({"t": "rug", "k": 3},
  // as Queries and the Profiler send them) or the editor's ({name: {kind,
  // value, field}}).
  // `where` is "logs" for the log store (the link Logs' "As SQL" makes).
  openSql = (sql, p, where) => {
    const logs = where === "logs";
    let t = tabs.find((x) => !x.sql.trim() && !!x.store === logs);
    if (t) { t.sql = sql; select(t.id); } else t = add(sql, null, logs ? "logs" : null);
    if (p) { try { t.params = editorParams(JSON.parse(p)); save(); drawParams(); } catch { /* a link with broken parameters opens without them */ } }
    history.replaceState(null, "", "#/sql");
  };
  const onLogs = () => tab()?.store === "logs";

  // ------------------------------------------------------------ the mode bar
  // Where a statement runs, as whom, and whether it may write.
  function drawMode() {
    if (onLogs()) {
      // The log store: every server's own lines, queries and load, read
      // with SELECT through POST /v1/logs/query. Nothing here writes.
      clear(modeBar, h("span", { class: "dt-pill ro" }, icon("logs"), "Log store"),
        h("span", { class: "dt-where ellipsis" }, h("b", {}, "the log store of every server"), h("span", { class: "muted" }, " · tables logs, queries and load · SELECT only, nothing written")),
        h("span", { class: "sep" }),
        h("button", { class: "btn sm", title: "Run this tab's statements on the database's tables instead", onclick: () => { delete tab().store; if (/^Log store/.test(tab().name)) tab().name = `Query ${tabs.indexOf(tab()) + 1}`; save(); drawTabs(); drawMode(); mark(); } }, icon("table"), "Query the tables instead"));
      modeBar.classList.add("ro");
      return;
    }
    const md = mode(), { name, env } = whereName(), r = reach(), w = state.whoami;
    const where = [h("b", { title: location.host }, name), env ? ` · ${env}` : isSandbox() ? h("span", { title: "The server names no environment and runs on this machine, so the workbench treats it as a sandbox" }, " · sandbox") : h("span", { title: "The server names no environment (--environment), so the workbench treats it as production" }, " · environment not set"),
      ` · ${r.coordinator ? fmt.plural(r.shards, "shard") : "one server"}`, w ? ` · ${w.name} · ${w.scope} key` : ""];
    let pillEl, note, action = null;
    if (md.why === "key" && signInNeeded()) { pillEl = h("span", { class: "dt-pill ro" }, icon("lock"), "Signed out"); note = hasToken() ? "the server refused this key" : "this server needs a key"; action = h("button", { class: "btn sm primary", onclick: () => bus.emit("unauthorized") }, "Enter a key"); }
    else if (md.why === "key") { pillEl = h("span", { class: "dt-pill ro" }, icon("lock"), "Read-only"); note = "this key can read only: writes need a write key"; }
    else if (md.readOnly) {
      pillEl = h("span", { class: "dt-pill ro" }, icon("lock"), "Read-only");
      note = "writes held for a check";
      action = h("button", { class: "btn sm", onclick: async () => { if (await askAllow()) { drawMode(); mark(); } } }, icon("unlock"), "Allow writes for 15 min…");
    } else if (md.allowed) {
      const left = Math.max(0, Math.ceil((md.allowed.until_ms - Date.now()) / 60000));
      pillEl = h("span", { class: "dt-pill on" }, icon("unlock"), `Writes on · ${left} min left`);
      note = h("span", { title: "Each write still waits for a check of the rows it changes" }, "why: ", h("i", {}, md.allowed.reason));
      action = h("button", { class: "btn sm", onclick: () => { endWrites(); drawMode(); mark(); toast("Writes are off again in this tab"); } }, icon("lock"), "End writes now");
    } else if (isSandbox()) { pillEl = h("span", { class: "dt-pill on" }, icon("unlock"), "Sandbox"); note = "writes are on here: each one still shows the rows it changes first"; }
    else { pillEl = h("span", { class: "dt-pill" }, icon("shield"), "Writes checked"); note = "each write waits for a check of the rows it changes"; }
    clear(modeBar, pillEl, h("span", { class: "dt-where ellipsis" }, where, h("span", { class: "muted" }, " · ", note)), h("span", { class: "sep" }), action);
    modeBar.classList.toggle("ro", md.readOnly);
  }
  let wasAllowed = !!writesAllowed();
  const modeTimer = setInterval(() => {
    const now = !!writesAllowed();
    if (wasAllowed && !now) toast("Writes are off again: the 15 minutes are over");
    wasAllowed = now; drawMode(); mark();
  }, 15000);
  const offs = ["cluster", "health", "whoami"].map((ev) => bus.on(ev, () => {
    drawMode();
    if (!traceTouched) { const want = isProduction() ? false : store.get("sql.trace", true); if (want !== trace) { trace = want; traceBox.checked = want; } }
  }));

  // ------------------------------------------------------------ parameters
  /** A query's parameters as the editor keeps them: {name: {kind, value, field}}. */
  function editorParams(j) {
    const out = {};
    for (const [n, v] of Object.entries(j || {})) {
      if (v && typeof v === "object" && !Array.isArray(v) && "kind" in v) out[n] = v;
      else if (v && typeof v === "object" && !Array.isArray(v) && typeof v.embed === "string") out[n] = { kind: "embed", value: v.embed, ...(v.field ? { field: v.field } : {}) };
      else if (typeof v === "string") out[n] = { kind: "text", value: v };
      else if (typeof v === "number") out[n] = { kind: "number", value: String(v) };
      else out[n] = { kind: "json", value: JSON.stringify(v) };
    }
    return out;
  }
  function drawParams() {
    const names = paramNames(ed.value);
    paramsBox.classList.toggle("hidden", !names.length);
    if (!names.length) return;
    const t = tab(); t.params ||= {};
    const vecs = cached().flatMap((x) => (x.short?.fields || []).filter((f) => f.type === "vector" && f.embed).map((f) => ({ table: x.table, field: f.name })));
    clear(paramsBox, h("div", { class: "small muted", style: { marginBottom: "6px" } }, "Parameters"), names.map((n) => {
      const p = (t.params[n] ||= { kind: "auto", value: "" });
      const inp = h("input", { class: "input sm mono grow", value: p.value, placeholder: p.kind === "embed" ? "text for the server to embed" : p.kind === "json" ? "[0.1, 0.2] or {…}" : "a value", oninput: () => { p.value = inp.value; save(); } });
      const field = h("select", { class: "select sm", title: "The vector field whose model embeds the text", onchange: () => { p.field = field.value; save(); } }, h("option", { value: "" }, "first embedded field"), vecs.map((v) => h("option", { value: v.field, selected: p.field === v.field }, `${v.table}.${v.field}`)));
      return h("div", { class: "row nw", style: { gap: "8px", marginBottom: "4px" } }, h("span", { class: "mono", style: { minWidth: "90px" } }, ":" + n),
        seg([["auto", "auto"], ["text", "text"], ["number", "number"], ["json", "JSON"], ["embed", "embed text"]], p.kind, (v) => { p.kind = v; save(); drawParams(); }), inp, p.kind === "embed" ? field : null);
    }));
  }
  function buildParams(sql) {
    const out = {}, t = tab();
    for (const n of paramNames(sql)) {
      const p = t.params?.[n] || { kind: "auto", value: "" };
      const v = p.value ?? "";
      if (!String(v).trim() && p.kind !== "text") throw new Error(`unbound parameter :${n}: the statement names :${n} but it has no value yet; fill it in under Parameters, below the editor`);
      // Text for a vector: the server embeds it with the field's model.
      const vecOf = p.kind === "auto" && !/^\s*[[{]/.test(v) ? embeddedFieldFor(sql, n) : null;
      if (vecOf) out[n] = { embed: v, field: vecOf };
      else if (p.kind === "embed") out[n] = p.field ? { embed: v, field: p.field } : { embed: v };
      else if (p.kind === "number") out[n] = Number(v);
      else if (p.kind === "text") out[n] = v;
      else if (p.kind === "json" || (p.kind === "auto" && /^\s*[[{]/.test(v))) { try { out[n] = JSON.parse(v); } catch (e) { throw new Error(`:${n} is not JSON: ${e.message}`); } }
      else out[n] = /^-?\d+(\.\d+)?(e-?\d+)?$/i.test(v.trim()) ? Number(v) : v;
    }
    return out;
  }

  /** The embedded vector field `similarity(field, :n)` (or maxsim) compares :n with, if any. */
  function embeddedFieldFor(sql, n) {
    const m = new RegExp(`\\b(?:similarity|maxsim)\\s*\\(\\s*(?:"?\\w+"?\\.)?"?(\\w+)"?\\s*,\\s*:${n}\\b`, "i").exec(sql);
    if (!m) return null;
    const f = cached().flatMap((x) => x.short?.fields || []).find((f) => f.name === m[1] && f.type === "vector" && f.embed);
    return f ? f.name : null;
  }

  // ------------------------------------------------------------ running
  async function run(explain = false) {
    if (running) { running.abort(); return; }
    const at = statementAt(ed.value, ed.selection);
    const range = [at.start, at.start + at.sql.length];
    let sql = at.sql.trimEnd().replace(/;\s*$/, "");
    let start = at.start + sql.length - sql.trimStart().length;
    sql = sql.trim();
    if (!sql) { toast("Write a statement first"); return; }
    // EXPLAIN written in the editor: the plan of the statement it explains.
    const ex = /^EXPLAIN(?:\s+ANALYZE)?\s+(?=\S)/i.exec(sql);
    if (ex) { explain = true; sql = sql.slice(ex[0].length); start += ex[0].length; }
    ed.clearMarks();
    const p = parseStatement(sql);
    const where = { sql, start, p };
    const logs = onLogs();
    if (logs && (p.write || explain)) {
      showError(p.write ? `The log store is read-only from here: ${p.kind} is not sent. Each server writes its own log store; it keeps lines for its retention and drops the oldest.` : "The log store answers SELECT only: there is no plan to show. Run the statement instead.", null);
      return;
    }
    if (!logs && !explain && /^\s*(SHOW|DESCRIBE|DESC)\s/i.test(sql)) {
      try {
        const local = localAnswer(sql, await tables().catch(() => cached()));
        if (local) { last = { j: local, sql, explain: false }; remember(sql, local, null); show("results"); return; }
      } catch (e) { remember(sql, null, e.message); showError(e.message, where); return; }
    }
    let params;
    try { params = buildParams(sql); } catch (e) { showError(e.message, where); return; }
    // A write: allowed in this tab, then checked (rows, shards, no undo, why).
    let reason;
    if (!explain && p.write) {
      const md = mode();
      if (md.why === "key") { showError(`${p.kind} needs a write key: this key (${state.whoami?.name || "unknown"}) can read only. Nothing was sent.`, null); return; }
      if (md.readOnly && !await askAllow(p)) return;
      drawMode();
      const edit = (fixed) => { const v = ed.value; ed.value = v.slice(0, range[0]) + (range[0] ? "\n" : "") + fixed.trim() + (v.slice(range[1]).startsWith(";") ? "" : ";") + v.slice(range[1]); tab().sql = ed.value; save(); ed.focus(); const i = ed.value.indexOf(fixed.trim()); if (i >= 0) ed.select(i + fixed.trim().length, i + fixed.trim().length); mark(); toast("The fix is in the editor: check it and run it again"); };
      const fixesFor = (msg) => {
        const word = wrongWord(msg);
        if (!word) return [];
        let names = suggested(msg);
        if (!names.length) names = nearNames(word, [...new Set(cached().flatMap((t) => [t.table, ...(t.short?.fields || []).map((f) => f.name)]))]).slice(0, 3);
        return names.slice(0, 3).map((n) => [`Write ${n} in its place`, replaceWord(sql, word, n)]);
      };
      const ok = await checkWrite({ sql, params, p, onEdit: edit, fixesFor });
      if (!ok) { mark(); return; }
      reason = ok.reason || undefined;
    }
    running = new AbortController();
    clear(runBtn, icon("stop"), p.write && !explain ? "Stop waiting" : "Stop"); runBtn.classList.add("danger");
    runBtn.title = p.write && !explain ? "Stops waiting for the answer. The write may still complete on the server." : "Stop waiting for the answer";
    out.classList.add("loading");
    const t0 = performance.now(), sent = Date.now();
    try {
      let j;
      if (logs) j = await api("/v1/logs/query", { body: { sql, params }, signal: running.signal });
      else if (explain) j = await api("/v1/explain", { body: { sql, params, hits: 5, profile: trace }, signal: running.signal });
      else if (!p.write && READ.test(stripStrings(sql))) {
        // More rows than a query returns: the first page, read as a scroll,
        // with how many there are and "Export all" (bigresult.js).
        try { j = await api("/v1/query", { body: { sql, params, trace, profile: trace, steps: true }, signal: running.signal }); }
        catch (e) { if (!tooBig(e)) throw e; j = await firstPage(sql, params, running.signal); }
      }
      else j = await api("/v1/sql", { body: { sql, params }, signal: running.signal, reason });
      j.roundtrip_ms = performance.now() - t0;
      last = { j, sql, explain, p, reason, sent, logs, params };
      remember(sql, j, null, p, reason, logs);
      if (/^\s*(CREATE|DROP|ALTER)\b/i.test(sql)) { invalidate(); tables(true).then(drawSide); toast(/^\s*CREATE/i.test(sql) ? "Created" : /^\s*ALTER/i.test(sql) ? "Table altered" : "Dropped", "ok"); }
      show(explain ? "plan" : j.written != null || p.write ? "results" : lastKind === "plan" ? "results" : lastKind);
    } catch (e) {
      if (e.name === "AbortError") toast(p.write && !explain ? "Stopped waiting. The write may still complete on the server: check with a SELECT before running it again." : "Stopped waiting: the server may still finish it", p.write ? "bad" : "info", p.write ? 9000 : 3800);
      else { remember(sql, null, e.message, p, reason); showError(e.message, where, e); }
    } finally {
      running = null; runBtn.classList.remove("danger"); out.classList.remove("loading"); mark();
    }
  }
  // Which servers a log-store query reached, and which did not answer.
  function serversLine(nodes) {
    const bad = nodes.filter((x) => x.error || (x.status && x.status !== "ok"));
    return h("span", { class: "small " + (bad.length ? "" : "muted"), title: nodes.map((x) => `${x.node || x.url}: ${x.error || `${fmt.plural(x.rows ?? 0, "row")} in ${fmt.ms(x.ms)}`}`).join("\n") },
      `${nodes.length - bad.length} of ${fmt.plural(nodes.length, "server")} answered`, bad.length ? h("span", { class: "dt-sc warn", style: { marginLeft: "6px" } }, `${bad.length} did not`) : null);
  }
  function remember(sql, j, error, p, reason, logs) {
    const hist = store.get("sql.history", []);
    hist.unshift({ sql, ts: Date.now(), ms: j?.took_ms, rows: j?.rows?.length ?? j?.written, error, kind: p?.kind, write: !!p?.write, reason, host: location.host, ...(logs ? { store: "logs" } : {}) });
    store.set("sql.history", hist.slice(0, 100));
  }
  // The error panel: what kind of error, the server's message whole, the
  // statement's line with the spot marked (also in the editor), the names
  // it may have meant as buttons that fix it, and the guide.
  function showError(msg, where, e) {
    last = null;
    // "query error: ..." and "schema error: ...": the heading says the kind.
    msg = String(msg || "").replace(/^(?:query|schema|sql) error: /i, "");
    const code = e instanceof ApiError ? e.status : e?.name === "TypeError" ? 0 : null;
    if (code === 0) msg = `No answer from the server at ${location.origin} (${msg}): it may be down or restarting, or the network dropped the request. Run the statement again in a moment.`;
    else if (/^HTTP \d+$/.test(msg)) msg = `The server answered ${msg} without saying why${code === 502 || code === 504 ? ": a proxy in front of it could not reach it, or the statement took too long" : ""}.`;
    clear(outTabs); clear(status);
    if (e instanceof ApiError && (e.status === 401 || e.status === 403)) { clear(outBody, failure(e, "This statement")); return; }

    const spot = where ? errorSpot(msg, where.sql) : null;
    const word = wrongWord(msg);
    let pos = null, excerpt = null;
    if (spot) {
      pos = where.start + spot.at;
      const [line, col] = lineCol(ed.value, pos);
      ed.markError(line, col);
      const text = ed.value.split("\n")[line - 1] ?? "";
      const len = Math.max(1, (spot.word || word) && ed.value.slice(pos, pos + (spot.word || word).length).toLowerCase() === (spot.word || word).toLowerCase() ? (spot.word || word).length : (/^\w+/.exec(text.slice(col - 1))?.[0].length || 1));
      const gutter = String(line).length;
      excerpt = h("pre", { class: "code wb-excerpt", "aria-label": `line ${line}, column ${col}` },
        h("span", { class: "muted" }, `${line} │ `), text, "\n",
        h("span", { class: "muted" }, `${" ".repeat(gutter)} │ `), h("span", { class: "wb-caret" }, " ".repeat(col - 1) + "^".repeat(len)));
    }

    // The names it may have meant: the server's own "(did you mean ...)",
    // or names of the catalog near the word.
    let names = suggested(msg);
    if (!names.length && word) {
      const all = [...new Set(cached().flatMap((t) => [t.table, ...(t.short?.fields || []).map((f) => f.name)]))];
      names = all.map((n) => [n, levenshtein(n.toLowerCase(), word.toLowerCase())]).filter(([, d]) => d > 0 && d <= (word.length < 5 ? 1 : Math.max(2, Math.floor(word.length / 3)))).sort((a, b) => a[1] - b[1]).slice(0, 3).map(([n]) => n);
    }
    // A fix to a read runs it again; a fix to a write only goes into the
    // editor, so that the write is checked again before it runs.
    const writes = !!where?.p?.write;
    const fix = h("div", { class: "row" });
    if (word) for (const n of names.slice(0, 4)) {
      fix.append(h("button", { class: "btn sm", title: writes ? `Write ${n} in place of ${word} in the editor. It does not run: check it again first` : `Write ${n} in place of ${word} and run it again`, onclick: () => {
        const v = ed.value;
        ed.value = pos != null && v.slice(pos, pos + word.length).toLowerCase() === word.toLowerCase()
          ? v.slice(0, pos) + (/^[A-Z]+$/.test(n) ? n : quoteName(n)) + v.slice(pos + word.length)
          : v.replace(new RegExp(`\\b${word}\\b`, "g"), quoteName(n));
        tab().sql = ed.value; save();
        if (writes) { ed.clearMarks(); ed.focus(); if (pos != null) ed.select(pos + quoteName(n).length, pos + quoteName(n).length); mark(); clear(fixNote, icon("okc"), ` ${n} is in the editor. ${runLabel(where.p)} to see the rows it changes before it runs.`); }
        else run();
      } }, writes ? icon("edit") : icon("wrench"), writes ? `Write ${n} in its place` : `Use ${n}`));
    }
    const fixNote = h("div", { class: "small muted row nw", style: { gap: "6px" } }, writes && fix.childElementCount ? [icon("shield"), ` This statement writes (${where.p.kind}${where.p.table ? " on " + where.p.table : ""}): the fix goes into the editor and is not run.`] : null);
    // A write that failed on the server's side (a shard did not answer, a
    // timeout) may have been done by the shards that did answer: a
    // coordinator does not undo them. Say so, and offer the SELECT that
    // shows the rows as they are now.
    const p = where?.p;
    // NEW-data-tools-5: an answer that says nothing was written is believed.
    const nothing = /nothing was written|no rows? (was|were) (written|changed)|0 rows changed/i.test(msg);
    const partial = writes && !nothing && p.kind !== "CREATE" && (code === 0 || code >= 500 || /\bshard\b.*\b(connect|timed? ?out|unavailable|no primary|takes no writes)\b/i.test(msg));
    const checkSql = partial && p.table ? `SELECT * FROM ${quoteName(p.table)}${p.where ? ` WHERE ${p.where}` : ""} LIMIT 200` : null;
    const partialNote = partial ? h("div", { class: "card dt-partial", role: "note" },
      h("div", { class: "row", style: { gap: "8px" } }, icon("warn"), h("b", {}, "Part of it may have been written")),
      h("div", { class: "small" }, code === 0
        ? "The answer did not arrive: the server may have run the statement. "
        : `${/\bshard\b/i.test(msg) ? "One shard failed. " : ""}On a cluster each shard's primary writes its own rows, and the shards that answered are not undone when another fails. `,
        "Look at the rows before you run it again", p.kind === "INSERT" ? ": a second INSERT of the same keys is refused or doubles them." : "."),
      checkSql ? h("div", { class: "row", style: { gap: "8px", marginTop: "8px" } },
        h("button", { class: "btn sm", title: checkSql, onclick: () => { add(checkSql, "Check " + p.table); run(); } }, icon("search"), "Look at the rows now"),
        h("span", { class: "small muted" }, "opens a SELECT in a new tab and runs it")) : null) : null;

    clear(outBody, h("div", { class: "stack wb-error", style: { padding: "14px", gap: "10px" } },
      h("div", { class: "row", style: { gap: "8px" } }, icon("bad"), h("b", {}, errorTitle(msg, code)),
        code ? h("span", { class: "pill", title: "the HTTP status of the answer" }, `HTTP ${code}`) : null,
        h("span", { class: "sep" }),
        h("button", { class: "btn ghost sm", title: "Copy the message", onclick: () => copy(msg, "Error copied") }, icon("copy"), "Copy")),
      h("div", { class: "error" }, msg),
      followsWho(msg) ? writeThere(followsWho(msg), "sql") : null,
      partialNote,
      excerpt,
      excerpt && !matchMedia("(max-width: 900px)").matches ? h("div", { class: "small muted" }, "The editor marks the same place.") : null,
      fix.childElementCount ? h("div", {}, h("div", { class: "small muted", style: { marginBottom: "6px" } }, "Did you mean"), fix) : null,
      fix.childElementCount ? fixNote : null,
      h("div", { class: "small muted" }, "The query language guide shows every clause and function with an example: ",
        h("a", { href: "https://github.com/chloetech-ai/chloedb/blob/main/docs/chloedb/query-language.md", target: "_blank", rel: "noopener" }, "Query language"),
        ". ", h("code", {}, "SHOW TABLES"), " and ", h("code", {}, "DESCRIBE table"), " list the tables and their columns. ",
        "The old query console's agent writes SQL from a question in English: ", h("a", { class: "wl-link", href: "#/classic" }, "Query console ›"))));
  }

  // ------------------------------------------------------------ the answer
  function show(kind) {
    lastKind = kind;
    if (!last) return;
    const { j, explain } = last;
    const kinds = explain ? [["plan", "Plan"], ["profile", "Profile"], ["json", "JSON"]] : j.written != null ? [["results", "Written"], ["json", "JSON"]] : j.local ? [["results", `Results ${fmt.n(j.rows.length)}`], ["json", "JSON"]] : [["results", `Results ${fmt.n(j.rows?.length || 0)}${j.capped ? "+" : ""}`], ["health", "Health"], ["chart", "Chart"], ["plan", "Plan"], ["profile", "Profile"], ["json", "JSON"]];
    if (kind === "trace") kind = "profile";
    if (!kinds.some(([k]) => k === kind)) kind = kinds[0][0];
    clear(outTabs, kinds.map(([k, l]) => h("button", { role: "tab", "aria-selected": String(k === kind), onclick: () => show(k) }, l)));
    clear(status, explain ? h("span", { class: "small" }, h("b", {}, j.kind), " · ", fmt.ms(j.took_ms), " · ", fmt.plural(j.rows, "row")) : j.local ? h("span", { class: "small muted", title: "SHOW and DESCRIBE are answered in the browser from GET /v1/tables" }, h("b", {}, fmt.plural(j.rows.length, "row")), " · from the catalog") : [answerStats(j), j.nodes?.length ? serversLine(j.nodes) : null],
      j.rows?.length ? h("span", { class: "row nw", style: { gap: "2px" } }, exportMenu(j, "chloedb-query")) : null);
    if (kind === "results" && (j.written != null || last.p?.write)) clear(outBody, written(j, last));
    else if (kind === "results") clear(outBody, j.capped ? cappedBanner(j, last.sql, last.params) : null, grid(j, j.capped ? { max: j.rows.length } : undefined));
    else if (kind === "health") clear(outBody, healthView(j, last.logs ? null : last.sql, last.params));
    else if (kind === "chart") clear(outBody, h("div", { style: { padding: "12px 14px" } }, resultChart(j)));
    else if (kind === "profile") {
      const steps = explain ? j.steps : j.stats?.steps;
      const cpu = steps?.some((x) => x.cpu_us != null);
      clear(outBody, h("div", { style: { padding: "12px 14px" } },
        h("div", { class: "row", style: { marginBottom: "10px" } }, h("span", { class: "small muted" }, cpu ? "Every step the query took, with its CPU time." : "Every step the query took. Tick “profile” and run it again for the CPU time of each and the request's trace."), h("span", { class: "sep" }),
          h("a", { class: "btn sm", href: "#/profile?tab=query&sql=" + encodeURIComponent(last.sql), title: "Run it in the Profiler, as many times as you like" }, icon("flame"), "Open in the Profiler")),
        profileView({ steps, trace: j.trace, took_ms: j.took_ms, roundtrip_ms: j.roundtrip_ms, plan: explain ? j.plan : j.stats?.plan, rows: explain ? j.rows : j.rows?.length, filter_docs: explain ? j.filter_docs : j.stats?.filter_docs, scored: explain ? j.scored : j.stats?.scored, profiled: cpu }, { compact: true })));
    }
    else if (kind === "json") clear(outBody, h("pre", { class: "code", style: { margin: "12px 14px", maxHeight: "60vh" } }, JSON.stringify(j.rows && j.rows.length > 200 ? { ...j, rows: j.rows.slice(0, 200), note: `${j.rows.length - 200} more rows left out` } : j, null, 2)));
    else if (kind === "plan") explainView(explain ? j : null);
  }
  // What a write did, in neutral colors: a write that worked is not good
  // news by itself. Whether the server kept it in its audit log, with the
  // reason, is read back from the log.
  function written(j, run) {
    const p = run.p || {};
    const ddl = ["CREATE", "DROP", "ALTER"].includes(p.kind);
    const seqs = j.seq_nos ? j.seq_nos.join(", ") : j.seq_no;
    const audit = h("div", { class: "small muted" });
    const box = h("div", { class: "stack", style: { padding: "16px", gap: "10px" } },
      h("div", { class: "banner dt-done" }, icon("info", "lg"), h("div", { class: "grow" },
        h("div", { class: "big" }, ddl ? `${p.kind} ${(p.what || "").toUpperCase()} ${p.table || ""}: done` : j.written != null ? `${fmt.plural(j.written, "row")} written` : `${p.kind || "Statement"} done`, h("span", { class: "muted", style: { fontWeight: 400 } }, p.table && !ddl ? ` · ${p.kind} on ${p.table}` : "")),
        h("div", { class: "small muted" }, `in ${fmt.ms(j.took_ms)}${seqs != null && seqs !== "" ? ` · sequence numbers ${seqs}` : ""}${j.skipped ? ` · ${fmt.n(j.skipped)} skipped` : ""} · at ${fmt.time(run.sent)}`),
        run.reason ? h("div", { class: "small" }, h("span", { class: "muted" }, "why: "), run.reason) : null)),
      audit,
      h("div", { class: "small muted" }, "There is no undo. ", p.table && (p.kind === "UPDATE" || p.kind === "DELETE" || p.kind === "INSERT") ? h("a", { href: "#", onclick: (e) => { e.preventDefault(); add(`SELECT * FROM ${quoteName(p.table)}${p.kind !== "INSERT" && p.where && p.kind !== "DELETE" ? ` WHERE ${p.where}` : ""} LIMIT 20;`, p.table); } }, "Look at the rows in a new tab") : null));
    // The audit log, where this key may read it.
    (async () => {
      try {
        const a = await api("/v1/admin/audit?" + new URLSearchParams({ kind: "change", limit: "30" }));
        const head = run.sql.slice(0, 60);
        const e = (a.entries || []).find((x) => x.ts_ms >= run.sent - 5000 && String(x.after?.statement || x.statement || x.detail || x.sql || "").includes(head));
        if (!e) clear(audit, icon("info"), " This server did not record the write in its audit log: an older version keeps only table changes there.");
        else clear(audit, icon("okc"), ` Kept in the audit log as a change by ${e.who}${e.reason ? ", with the reason" : ""}`, e.reason ? "" : run.reason ? " (this server keeps no reason)" : ", with no reason given", ". ", h("a", { href: "#/access" }, "Access and audit"));
      } catch { /* a key that cannot read the audit log */ }
    })();
    return box;
  }
  async function explainView(j) {
    if (!j) {
      clear(outBody, h("div", { class: "small muted", style: { padding: "14px" } }, "Explaining…"));
      try { j = await api("/v1/explain", { body: { sql: last.sql, params: buildParams(last.sql), hits: 5 } }); }
      catch (e) { clear(outBody, h("div", { class: "error", style: { margin: "14px" } }, e.message)); return; }
    }
    const hits = j.hits?.length ? h("div", { class: "tbl-wrap" }, grid({ columns: Object.keys(j.hits[0]), rows: j.hits.map((x) => Object.values(x)) })) : null;
    clear(outBody, h("div", { class: "plan-view" },
      h("div", { class: "grid g2" },
        h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Plan"), h("span", { class: "sub" }, "the statement as bound: rows flow up")), h("div", { class: "bd" }, planTree(j.tree))),
        h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "What ran"), h("span", { class: "sub" }, `${fmt.n(j.filter_docs)} passed the filter · ${fmt.n(j.scored)} scored`)), h("div", { class: "bd" }, segments(j.plan)))),
      hits ? h("section", { class: "card", style: { marginTop: "14px" } }, h("div", { class: "hd" }, h("h3", {}, "The top rows and the values they were ranked by")), h("div", { class: "bd flush" }, hits)) : null,
      h("details", { class: "more" }, h("summary", {}, "The plan as JSON, which runs on its own (POST /v1/plan)"),
        h("div", { class: "row", style: { margin: "8px 0" } }, h("button", { class: "btn sm", onclick: () => copy(JSON.stringify(j.ir), "Plan copied") }, icon("copy"), "Copy")),
        h("pre", { class: "code", style: { maxHeight: "40vh" } }, JSON.stringify(j.ir, null, 2))),
      h("details", { class: "more" }, h("summary", {}, "The query as bound to the schema"), h("pre", { class: "code", style: { maxHeight: "40vh" } }, j.logical))));
  }

  // ------------------------------------------------------------ the library
  function saveQuery() {
    const inp = h("input", { class: "input", value: tab().name, style: { width: "100%" } });
    const m = modal({ title: "Save this query", body: [inp, sqlBlock(ed.value, { style: { maxHeight: "30vh" } })], foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), h("button", { class: "btn primary", onclick: go }, "Save")] });
    inp.onkeydown = (e) => { if (e.key === "Enter") go(); };
    function go() { const saved = store.get("sql.saved", []).filter((x) => x.name !== inp.value.trim()); saved.unshift({ name: inp.value.trim() || "Query", sql: ed.value, params: tab().params, ts: Date.now() }); store.set("sql.saved", saved); tab().name = inp.value.trim() || tab().name; save(); drawTabs(); m.close(); toast("Saved in this browser", "ok"); }
  }
  function library() {
    const q = h("input", { class: "input", placeholder: "Search the SQL…", style: { width: "100%" } });
    const list = h("div", { class: "stack", style: { gap: "8px" } });
    let which = "saved";
    const tabsEl = seg([["saved", "Saved"], ["history", "History"]], which, (v) => { which = v; draw(); });
    const d = drawer({ title: "Library", sub: "kept in this browser · writes are marked", wide: true, body: [h("div", { class: "row" }, tabsEl, h("span", { class: "sep" })), q, list] });
    q.oninput = draw;
    function draw() {
      const needle = q.value.toLowerCase();
      const items = (which === "saved" ? store.get("sql.saved", []) : store.get("sql.history", [])).filter((x) => !needle || x.sql.toLowerCase().includes(needle) || x.name?.toLowerCase().includes(needle));
      clear(list, items.length ? items.map((x, i) => h("div", { class: "lib-it" },
        h("div", { class: "row between" }, h("span", { class: "row nw", style: { gap: "8px" } }, which === "history" ? h("span", { class: "dt-kind " + (x.error ? "bad" : x.write ? "write" : "read") }, x.error ? "failed" : x.write ? "write" : "read") : null, x.store === "logs" ? h("span", { class: "dt-kind", title: "Ran on the log store" }, "log store") : null, h("b", {}, x.name || fmt.datetime(x.ts))),
          h("span", { class: "small muted" }, x.error ? h("span", { class: "dim" }, x.error.slice(0, 80)) : x.ms != null ? `${fmt.ms(x.ms)} · ${fmt.plural(x.rows ?? 0, "row")}${x.write ? " written" : ""}` : "", x.name ? " · " + fmt.ago(x.ts) : "", x.host && x.host !== location.host ? ` · on ${x.host}` : "")),
        x.reason ? h("div", { class: "small dim" }, "why: ", x.reason) : null,
        sqlBlock(x.sql, { style: { maxHeight: "120px", marginTop: "6px" } }),
        h("div", { class: "row", style: { marginTop: "6px" } },
          h("button", { class: "btn sm primary", onclick: () => { d.close(); const t = add(x.sql, x.name, x.store); if (x.params) { t.params = x.params; drawParams(); } } }, "Open in a tab"),
          h("button", { class: "btn sm ghost", onclick: () => copy(x.sql, "SQL copied") }, icon("copy"), "Copy"),
          which === "saved" ? h("button", { class: "btn sm ghost danger", onclick: () => { const s = store.get("sql.saved", []); s.splice(i, 1); store.set("sql.saved", s); draw(); } }, icon("trash"), "Remove") : null)))
        : empty(which === "saved" ? "plus" : "clock", which === "saved" ? "Nothing saved yet" : "No history yet", which === "saved" ? "The + above the editor saves a query here." : "Every statement you run is kept here."));
    }
    draw();
  }

  // ------------------------------------------------------------ the sidebar
  const filter = h("input", { class: "input sm", placeholder: "Filter fields…", style: { width: "100%" }, oninput: () => drawSide() });
  const open = new Set(store.get("sql.open", []));
  function drawSide() {
    const ts = cached();
    const q = filter.value.toLowerCase();
    const first = ts.find((t) => open.has(t.table)) || firstMine(ts);
    clear(side, h("div", { class: "wb-sh" }, h("b", {}, "Tables"), h("span", { class: "sep" }), h("button", { class: "btn ghost icon sm", title: "Read the tables again", onclick: () => tables(true).then(drawSide) }, icon("refresh")),
      h("button", { class: "btn ghost sm wb-side-close", title: "Back to the editor", onclick: () => sideOpen(false) }, icon("x"), "Close")), filter,
      h("div", { class: "tree" }, folded(q ? ts.filter((t) => t.table.toLowerCase().includes(q) || (t.short?.fields || []).some((f) => f.name.toLowerCase().includes(q))) : ts, (t) => {
        const fs = (t.short?.fields || []).filter((f) => !q || f.name.toLowerCase().includes(q) || t.table.toLowerCase().includes(q));
        if (q && !fs.length) return null;
        const isOpen = open.has(t.table) || !!q || ts.length === 1;
        return h("div", { class: "tt" },
          h("div", { class: "tt-h", draggable: "true", title: "Click to open · drag into the editor", ondragstart: (e) => e.dataTransfer.setData("text/plain", quoteName(t.table)), onclick: () => { if (open.has(t.table)) open.delete(t.table); else open.add(t.table); store.set("sql.open", [...open]); drawSide(); } },
            icon(isOpen ? "down" : "right"), icon("table"), h("span", { class: "grow ellipsis" }, t.table), h("span", { class: "small muted", title: "Its fields; the rows are counted on the Tables page" }, fmt.plural(fs.length, "field")),
            h("button", { class: "btn ghost icon sm", title: `SELECT * FROM ${t.table} in a new tab`, onclick: (e) => { e.stopPropagation(); add(`SELECT * FROM ${quoteName(t.table)} LIMIT 20;`, t.table); run(); } }, icon("play"))),
          isOpen ? h("div", { class: "tt-f" }, fs.map((f) => h("div", { class: "fi", draggable: "true", title: `${f.name}: drag into the editor, or click to insert`, ondragstart: (e) => e.dataTransfer.setData("text/plain", quoteName(f.name)), onclick: () => ed.insert(quoteName(f.name)) },
            typeBadge(f), h("span", { class: "grow ellipsis" }, f.name), t.short.key === f.name ? h("span", { class: "badge accent", title: "the primary key" }, "key") : null, f.multi ? h("span", { class: "badge" }, "[]") : null))) : null);
      }, { key: "sql", nameOf: (t) => t.table, open: !!q, label: "Tables of apps" })),
      h("div", { class: "wb-sh", style: { marginTop: "14px" } }, h("b", {}, "Snippets"), h("span", { class: "small muted" }, first ? "for " + first.table : "")),
      h("div", { class: "snips" }, snippets(first).map(([name, what, sql]) => h("div", { class: "snip", draggable: "true", title: "Drag into the editor, or click to open in a new tab", ondragstart: (e) => e.dataTransfer.setData("text/plain", sql), onclick: () => add(sql, name) }, h("b", {}, name), h("div", { class: "small muted" }, what)))));
  }

  drawTabs(); drawParams(); drawMode();
  clear(outBody, empty("sql", "Run a query to see its answer", matchMedia("(hover: none)").matches ? "Run runs the statement the cursor is in; Explain shows its plan. A write waits for a check of the rows it changes." : "Ctrl+Enter runs the statement at the caret; Ctrl+Shift+Enter explains it. A write waits for a check of the rows it changes."));
  tables().then(() => { drawSide(); drawParams(); if (!tab().sql.trim() && cached()[0]) { ed.value = `SELECT * FROM ${quoteName(firstMine(cached()).table)} LIMIT 20;`; tab().sql = ed.value; save(); } }).catch((e) => clear(side, failure(e, "The tables")));
  if (r.params.get("q")) openSql(r.params.get("q"), r.params.get("p"), r.params.get("store"));
  setTimeout(() => { ed.focus(); mark(); }, 50);
  return () => { openSql = null; running?.abort(); clearInterval(modeTimer); offs.forEach((f) => f()); };
}

