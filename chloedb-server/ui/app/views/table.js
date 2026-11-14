// A home per table (#/table/<name>): what a person who owns the data
// comes for, in one place. Search, the box that works on the first try
// (some of the words, ranked; by meaning too once the table has vectors,
// which one click adds); Data, its rows; Quality, how filled each field
// is; Used by, the keys, saved searches, statements and apps that read
// it; Storage, its index (the inspector). A search is kept as a saved
// search (a stored function, an endpoint, an agent's tool) from here.
// #/table lists the tables, the user's own first.

import { h, icon, clear, api, card, empty, failure, fmt, toast, copy, seg, gate, can, sqlBlock, store, debounce, table, bus, ApiError } from "../core.js";
import { tables, cached, table as tableOf, isStored, isTestModel, typeBadge, invalidate } from "../catalog.js";
import { ownerOf, isMine, folded } from "../owners.js";
import { quoteName } from "../sqledit.js";
import { grid, answerStats, exportMenu } from "../results.js";
import { useFromCode, saveSearch, slug } from "./saved.js";
import { bestText } from "./builder.js";
import { NOT_PROSE } from "./models.js";
import inspector from "./inspector.js";

export default { id: "table", title: "Tables", icon: "table", group: "Build", range: false, keywords: "tables table home search find rows data quality filled empty used by keys storage index meaning vectors semantic", render, onParams };

const TABS = [["search", "Search"], ["data", "Data"], ["quality", "Quality"], ["used", "Used by"], ["storage", "Storage"]];
let again = null;
function onParams(r) { again?.(r); }

const searchable = (f) => f && f.type === "text" && f.indexed !== false;
/** The text fields a search looks in: prose, not addresses, the best first, at most four. */
export function proseFields(t) {
  const fs = (t?.short?.fields || []).filter(searchable);
  const prose = fs.filter((f) => !NOT_PROSE.test(f.name));
  const best = bestText(prose.length ? prose : fs);
  const title = prose.find((f) => /^(title|name|subject|headline)$/i.test(f.name));
  return [...new Set([best, title, ...prose].filter(Boolean))].slice(0, 4);
}
/** The vector fields a table embeds from its text (searchable by words). */
const meaningFields = (t) => (t?.short?.fields || []).filter((f) => f.type === "vector" && f.embed && !f.multi && !/_(next|prev)$/.test(f.name));
const TITLE = /^(title|name|subject|headline)$/i;
const LONG = /(body|content|text|description|desc|summary|abstract|details|article|message|review|notes?)/i;
/**
 * The fields an answer shows: {cols, title, text}. The key, a title, the
 * text searched, a few more; never vectors. A searched field that is not
 * kept (a TEXT made from others, `from`) reads NULL, so the stored fields
 * it is made from are shown in its place, as are those a vector is
 * embedded from.
 */
export function shownFields(t, text, vec = null) {
  const fs = (t?.short?.fields || []).filter((f) => !["vector", "sparse", "edges"].includes(f.type) && isStored(f));
  const has = (n) => fs.some((f) => f.name === n);
  const key = t?.short?.key;
  const sourcesOf = (f) => [].concat(f?.from || f?.embed?.from || []).flatMap((x) => String(x).split(/\s*,\s*/));
  const searched = [...new Set([...text.flatMap((f) => (isStored(f) ? [f.name] : sourcesOf(f))), ...(vec ? sourcesOf(vec) : [])].filter((n) => has(n) && n !== key))];
  const prose = fs.filter((f) => f.type === "text" && f.name !== key).map((f) => f.name);
  const title = [...searched, ...prose].find((n) => TITLE.test(n)) || null;
  const rest = (n) => n !== title;
  const body = searched.filter(rest).find((n) => LONG.test(n)) || searched.find(rest) || prose.filter(rest).find((n) => LONG.test(n)) || (title ? null : prose[0]) || null;
  const others = fs.filter((f) => f.type !== "text" && f.type !== "json").map((f) => f.name);
  const cols = [...new Set([key, title, body, ...others].filter((n) => n && has(n)))].slice(0, 7);
  return { cols, title, text: body };
}

/**
 * The search a box runs: {sql, params, body, fnParams}. `mode`: words,
 * meaning or both. Words alone over one field rank by bm25 with a guard
 * (all the words while 20 rows have them, else fewer), so a sentence
 * finds rows; over several, a row needs a word of one of them and the
 * scores add up. Meaning ranks every row by similarity; both fuse the
 * rankings (rrf). `body` and `fnParams` are the same search as a stored
 * function's (the words :q, the vector :v, the count :k).
 */
export function searchSql(t, { q, fields, vec, mode, limit = 20 }) {
  const shown = shownFields(t, fields, mode !== "words" ? vec : null);
  const cols = shown.cols.map(quoteName);
  const from = quoteName(t.table);
  const W = fields.map((f) => quoteName(f.name));
  let where = "", order;
  const words = mode !== "meaning" && W.length;
  const meaning = mode !== "words" && vec;
  if (words && meaning) order = `rrf(${W.map((f) => `bm25(${f}, :q)`).join(", ")}, similarity(${quoteName(vec.name)}, :v))`;
  else if (meaning) order = `similarity(${quoteName(vec.name)}, :v)`;
  else if (W.length === 1) order = `bm25(${W[0]}, :q, min => 1, min_rows => 20)`;
  else if (W.length) { where = `\nWHERE ${W.map((f) => `MATCH(${f}, :q, min => 1)`).join("\n   OR ")}`; order = W.map((f) => `bm25(${f}, :q)`).join(" + "); }
  else return null;
  const head = `SELECT ${[...cols, "score()"].join(", ")}\nFROM ${from}${where}\nORDER BY ${order} DESC`;
  const params = {};
  if (words) params.q = q;
  if (meaning) params.v = { embed: q, field: vec.name };
  const fnParams = [];
  if (words) fnParams.push({ name: "q", ty: "TEXT" });
  if (meaning) fnParams.push({ name: "v", ty: `VECTOR(${vec.dim || ""})`.replace("()", "") });
  fnParams.push({ name: "k", ty: "INT", default: 10 });
  return { sql: `${head}\nLIMIT ${limit}`, params, body: `${head}\nLIMIT :k`, fnParams, shown };
}

// ------------------------------------------------------------ a table's profile
/**
 * What each field holds, for the Quality tab: [{filled, more}] in the
 * order of `fs`. The counts and ranges of many fields go in one statement
 * (aggregates side by side, a batch of fields at a time), and the top
 * values of each keyword field in one of their own, a few statements at a
 * time: a table of 90 fields asks about 30 statements, not 250 at once.
 * The key is known to be different in every row, so it is not counted.
 */
export function profileSql(t, fs, { batch = 24 } = {}) {
  const tq = quoteName(t.table), key = t.short?.key;
  const parts = [], tops = [];
  fs.forEach((f, i) => {
    const c = quoteName(f.name);
    const add = (kind, expr) => parts.push({ i, kind, expr });
    if (f.name !== key) add("filled", f.multi || f.type === "json" ? `COUNT(*) FILTER (WHERE ${c} IS NOT NULL)` : `COUNT(${c})`);
    if (f.multi) { if (f.type === "keyword") tops.push({ i, sql: `SELECT ${c}, COUNT(*) AS n FROM ${tq} GROUP BY ${c} ORDER BY n DESC LIMIT 3` }); return; }
    if (/^(date|time)/.test(f.type)) { add("min", `MIN(${c})`); add("max", `MAX(${c})`); }
    else if (/^(int|float)/.test(f.type)) { add("min", `MIN(${c})`); add("max", `MAX(${c})`); add("avg", `AVG(${c})`); }
    else if (f.type === "keyword") {
      if (f.name !== key) { add("distinct", `COUNT(DISTINCT ${c})`); tops.push({ i, sql: `SELECT ${c}, COUNT(*) AS n FROM ${tq} GROUP BY ${c} ORDER BY n DESC LIMIT 3` }); }
    } else if (f.type === "text") { add("len", `AVG(LENGTH(${c}))`); add("maxlen", `MAX(LENGTH(${c}))`); add("blank", `COUNT(*) FILTER (WHERE ${c} = '')`); }
  });
  // Whole fields to a statement, at most `batch` aggregates in each.
  const batches = [];
  let cur = [];
  for (const p of parts) {
    if (cur.length >= batch && cur[cur.length - 1].i !== p.i) { batches.push(cur); cur = []; }
    cur.push(p);
  }
  if (cur.length) batches.push(cur);
  return { batches: batches.map((b) => ({ parts: b, sql: `SELECT ${b.map((p) => p.expr).join(", ")} FROM ${tq}` })), tops };
}

/** Runs `jobs` (functions that return a promise), `limit` at a time. */
export async function bounded(jobs, limit = 4) {
  const out = new Array(jobs.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(limit, jobs.length) }, async () => {
    while (next < jobs.length) { const k = next++; out[k] = await jobs[k](); }
  }));
  return out;
}

const profiles = new Map();
/** The profile of a table's fields, kept a few minutes per table and row count. */
async function profile(t, fs, n) {
  const id = `${t.table}:${n}:${fs.map((f) => f.name).join(",")}`;
  const kept = profiles.get(id);
  if (kept && Date.now() - kept.at < 5 * 60e3) return kept.stats;
  const q = (sql) => api("/v1/query", { body: { sql }, timeout: 15000 }).then((j) => j.rows || []).catch(() => null);
  const { batches, tops } = profileSql(t, fs);
  const stats = fs.map(() => ({ filled: null, more: null, agg: {} }));
  const put = (p, v) => { if (v !== undefined) stats[p.i].agg[p.kind] = v; };
  const jobs = [
    ...batches.map((b) => async () => {
      const rows = await q(b.sql);
      if (rows) { b.parts.forEach((p, k) => put(p, rows[0]?.[k])); return; }
      // A batch the server refused: field by field, so one field does not hide the rest.
      const one = [...new Set(b.parts.map((p) => p.i))];
      await bounded(one.map((i) => async () => {
        const ps = b.parts.filter((p) => p.i === i);
        const r = await q(`SELECT ${ps.map((p) => p.expr).join(", ")} FROM ${quoteName(t.table)}`);
        if (r) ps.forEach((p, k) => put(p, r[0]?.[k]));
      }), 2);
    }),
    ...tops.map((x) => async () => { stats[x.i].top = await q(x.sql); }),
  ];
  await bounded(jobs, 4);
  // The shapes the tab reads: filled, and `more` as each type's answer was.
  fs.forEach((f, i) => {
    const s = stats[i], a = s.agg;
    s.filled = f.name === t.short?.key ? n : a.filled ?? null;
    if (f.multi) s.more = s.top || null;
    else if (/^(date|time)/.test(f.type)) s.more = "min" in a ? [[a.min, a.max]] : null;
    else if (/^(int|float)/.test(f.type)) s.more = "min" in a ? [[a.min, a.max, a.avg]] : null;
    else if (f.type === "keyword") s.more = f.name === t.short?.key ? [[[n]], null] : [a.distinct != null ? [[a.distinct]] : null, s.top || null];
    else if (f.type === "text") s.more = "len" in a ? [[[a.len, a.maxlen]], [[a.blank]]] : null;
  });
  profiles.set(id, { at: Date.now(), stats });
  return stats;
}

// ------------------------------------------------------------ the page
function render(page, r) {
  const body = h("div", { class: "stack" });
  page.append(body);
  let stop = null, alive = true;

  function show(route) {
    r = route;
    stop?.(); stop = null;
    const name = r.rest?.[0];
    if (!name) return index();
    tables().then(() => {
      const t = tableOf(name);
      if (!alive) return;
      if (!t) { clear(body, card({ body: empty("table", `No table ${name}`, "It may have been dropped, or your key does not read it.", h("a", { class: "btn", href: "#/table" }, "All tables")) })); return; }
      tablePage(t, r.params.get("tab") || store.get("table.tab", "search"));
    }).catch((e) => clear(body, failure(e, "The tables")));
  }
  again = show;

  // ---------------------------------------------------------- every table
  async function index() {
    clear(body, h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Tables"), h("p", {}, "Your tables first: search one, look at its rows, see who uses it.")),
      h("div", { class: "tools" }, gate(h("a", { class: "btn", href: "#/import" }, icon("import"), "Import a file"), "write"), gate(h("a", { class: "btn", href: "#/schema?new=1" }, icon("plus"), "Design a table"), "admin"))),
    h("div", { class: "small muted", style: { padding: "12px" } }, "Reading the tables…"));
    let ts;
    try { ts = await tables(true); } catch (e) { clear(body.lastChild, failure(e, "The tables")); return; }
    const sizes = {}, idx = {};
    try { const j = await api("/v1/index/tables", { timeout: 4000 }); for (const x of Array.isArray(j) ? j : j.tables || []) { sizes[x.table] = x.docs; idx[x.table] = x; } } catch { /* a server without the listing */ }
    const mine = ts.filter((t) => isMine(t.table));
    await Promise.all(mine.filter((t) => sizes[t.table] == null).slice(0, 30).map((t) => api("/v1/query", { body: { sql: `SELECT COUNT(*) FROM ${quoteName(t.table)}` }, timeout: 4000 }).then((j) => { sizes[t.table] = j.rows?.[0]?.[0]; }).catch(() => {})));
    if (!alive || r.rest?.[0]) return;
    const use = (x) => x ? [x.bytes != null ? fmt.bytes(x.bytes) : null, x.reads_per_min ? `${fmt.si(x.reads_per_min)} reads/min` : "no reads lately", x.writes_per_min ? `${fmt.si(x.writes_per_min)} writes/min` : x.last_write_ms ? `last write ${fmt.ago(x.last_write_ms)}` : null].filter(Boolean).join(" · ") : "";
    const tile = (t) => {
      const fs = t.short?.fields || [];
      const text = proseFields(t), vecs = meaningFields(t);
      const o = ownerOf(t.table);
      return h("a", { class: "tbl-tile", href: `#/table/${encodeURIComponent(t.table)}` },
        h("div", { class: "row nw" }, icon("table"), h("b", { class: "grow ellipsis" }, t.table), sizes[t.table] != null ? h("span", { class: "small muted" }, fmt.si(sizes[t.table]) + " rows") : null),
        h("div", { class: "small dim ellipsis" }, `${fmt.plural(fs.length, "field")}${text.length ? " · search " + text.map((f) => f.name).join(", ") : " · no text to search"}`),
        idx[t.table] ? h("div", { class: "small muted ellipsis", title: "on disk, and the use of the last 5 minutes" }, use(idx[t.table])) : null,
        h("div", { class: "row nw small", style: { gap: "6px", marginTop: "4px" } },
          text.length ? h("span", { class: "badge" }, "words") : null, vecs.length ? (vecs.every((v) => isTestModel(v.embed?.model)) ? h("span", { class: "badge muted", title: `A test model (${vecs[0].embed.model}): hashed words, no meaning` }, "test vectors") : h("span", { class: "badge accent" }, "meaning")) : text.length ? h("span", { class: "badge muted", title: "Add vectors to search by meaning" }, "no meaning yet") : null,
          o ? h("span", { class: "badge", title: o.about }, o.owner) : null));
    };
    // The list: every table a row, sortable by size and use, apps' tables
    // marked by their app rather than folded away.
    const listView = () => table([
      { key: "table", label: "Table", render: (t) => h("a", { href: `#/table/${encodeURIComponent(t.table)}`, class: "mono" }, t.table) },
      { key: "owner", label: "Owner", sort: (t) => ownerOf(t.table)?.owner || "", render: (t) => { const o = ownerOf(t.table); return o ? h("span", { class: "badge", title: o.about }, o.owner) : h("span", { class: "muted" }, "yours"); } },
      { key: "docs", label: "Rows", num: true, sort: (t) => sizes[t.table], render: (t) => fmt.n(sizes[t.table]) },
      { key: "bytes", label: "On disk", num: true, sort: (t) => idx[t.table]?.bytes, render: (t) => fmt.bytes(idx[t.table]?.bytes) },
      { key: "reads", label: "Reads/min", num: true, sort: (t) => idx[t.table]?.reads_per_min, render: (t) => idx[t.table] ? fmt.si(idx[t.table].reads_per_min || 0) : "–" },
      { key: "writes", label: "Writes/min", num: true, sort: (t) => idx[t.table]?.writes_per_min, render: (t) => idx[t.table] ? fmt.si(idx[t.table].writes_per_min || 0) : "–" },
      { key: "last", label: "Last write", num: true, sort: (t) => idx[t.table]?.last_write_ms, render: (t) => idx[t.table]?.last_write_ms ? fmt.ago(idx[t.table].last_write_ms) : "–" },
      { key: "fields", label: "Fields", num: true, sort: (t) => (t.short?.fields || []).length, render: (t) => String((t.short?.fields || []).length) },
      { key: "search", label: "Search", sortable: false, render: (t) => h("span", { class: "row nw", style: { gap: "4px" } }, proseFields(t).length ? h("span", { class: "badge" }, "words") : h("span", { class: "muted small" }, "none"), meaningFields(t).some((v) => !isTestModel(v.embed?.model)) ? h("span", { class: "badge accent" }, "meaning") : meaningFields(t).length ? h("span", { class: "badge muted" }, "test vectors") : null) },
    ], { rows: ts, sortKey: "docs", desc: true, onRow: (t) => { location.hash = `#/table/${encodeURIComponent(t.table)}`; } });
    let how = store.get("tables.how", "tiles");
    const host = h("div");
    const draw = () => clear(host, how === "list" ? h("section", { class: "card" }, h("div", { class: "bd flush" }, listView())) : h("div", { class: "tbl-tiles" }, folded(ts, tile, { key: "tablehome", nameOf: (t) => t.table, label: "Tables of apps" })));
    const total = Object.values(idx).reduce((a, x) => a + (x.bytes || 0), 0);
    const head = h("div", { class: "row", style: { gap: "12px", flexWrap: "wrap" } },
      h("span", { class: "small muted grow" }, `${fmt.plural(ts.length, "table")}${mine.length !== ts.length ? `, ${mine.length} yours` : ""}${total ? ` · ${fmt.bytes(total)} on disk` : ""}${Object.keys(idx).length ? " · use over the last 5 minutes" : ""}`),
      seg([["tiles", "Tiles"], ["list", "List"]], how, (v) => { how = v; store.set("tables.how", v); draw(); }, { "aria-label": "Show as" }));
    draw();
    clear(body.lastChild, ts.length ? h("div", { class: "stack" }, head, host) : card({ body: empty("table", "No tables yet", "Import a file to make one, or design one.", h("div", { class: "row" }, h("a", { class: "btn primary", href: "#/import" }, icon("import"), "Import a file"), h("a", { class: "btn", href: "#/start" }, icon("rocket"), "Start here"))) }));
  }

  // ---------------------------------------------------------- one table
  function tablePage(t, tab) {
    if (!TABS.some((x) => x[0] === tab)) tab = "search";
    store.set("table.tab", tab);
    const o = ownerOf(t.table);
    const tabsEl = h("div", { class: "tabs", role: "tablist" }, TABS.map(([k, label]) => h("button", { role: "tab", "aria-selected": String(k === tab), onclick: () => { location.hash = `#/table/${encodeURIComponent(t.table)}?tab=${k}`; } }, label)));
    const pane = h("div", { class: "stack" });
    const rows = h("span", { class: "small muted" });
    clear(body,
      h("div", { class: "row" }, h("a", { class: "btn ghost sm", href: "#/table" }, icon("left"), "All tables")),
      h("div", { class: "page-head" }, h("div", {}, h("h1", { class: "mono" }, t.table), h("p", {}, o ? `Kept by ${o.owner}: ${o.about}. ` : "", rows)),
        h("div", { class: "tools" }, h("a", { class: "btn sm", href: `#/sql?q=${encodeURIComponent(`SELECT * FROM ${quoteName(t.table)} LIMIT 20;`)}` }, icon("sql"), "SQL"), h("a", { class: "btn sm", href: `#/builder?table=${encodeURIComponent(t.table)}` }, icon("builder"), "Builder"), h("a", { class: "btn sm", href: `#/schema?table=${encodeURIComponent(t.table)}` }, icon("schema"), "Schema"))),
      tabsEl, pane);
    api("/v1/query", { body: { sql: `SELECT COUNT(*) FROM ${quoteName(t.table)}` }, timeout: 5000 }).then((j) => { rows.textContent = `${fmt.n(j.rows?.[0]?.[0] ?? 0)} rows · ${fmt.plural((t.short?.fields || []).length, "field")}`; }).catch(() => {});
    stop = { search: searchTab, data: dataTab, quality: qualityTab, used: usedTab, storage: storageTab }[tab](pane, t) || null;
  }

  // ---------------------------------------------------------- Search
  function searchTab(pane, t) {
    const text = proseFields(t);
    let vecs = meaningFields(t);
    const key = `table.search.${t.table}`;
    const saved = store.get(key, {});
    let picked = new Set((saved.fields || text.map((f) => f.name)).filter((n) => text.some((f) => f.name === n)));
    if (!picked.size && text[0]) picked.add(text[0].name);
    // A test model's vectors rank by hashed words, not meaning: words first.
    let mode = saved.mode || (vecs.length && text.length ? (isTestModel(vecs[0].embed?.model) ? "words" : "both") : vecs.length ? "meaning" : "words");
    const inp = h("input", { class: "input search-big", placeholder: text.length || vecs.length ? `Search ${t.table}: a question or some words` : "This table has no text to search", value: saved.q || "", "aria-label": `Search ${t.table}` });
    const modeBox = h("div");
    const fieldsBox = h("div", { class: "row nw small", style: { gap: "6px", flexWrap: "wrap" } });
    const meaningBox = h("div");
    const out = h("div");
    const tail = h("div", { class: "stack" });
    let running = null, last = null;
    function drawControls() {
      vecs = meaningFields(tableOf(t.table) || t);
      if (!vecs.length && mode !== "words") mode = "words";
      if (!text.length && mode !== "meaning") mode = vecs.length ? "meaning" : "words";
      clear(modeBox, seg([["words", "Words", "Rows with your words, the more and rarer the higher"], ["meaning", "Meaning", vecs.length ? (isTestModel(vecs[0].embed.model) ? `${vecs[0].embed.model} is a test model: it hashes words and knows no meaning (change it in Models)` : `Rows close in meaning (${vecs[0].embed.model})`) : "Add meaning first"], ["both", "Both", "Words and meaning fused: the best of the two rankings"]].filter(([k]) => (k === "words" ? text.length : vecs.length) && (k !== "both" || text.length)), mode, (v) => { mode = v; keep(); go(); }, { "aria-label": "Search by" }));
      clear(fieldsBox, mode === "meaning" ? h("span", { class: "muted" }, `by meaning, with ${vecs[0]?.name} (${vecs[0]?.embed?.model})`) : [h("span", { class: "muted" }, "in"), text.map((f) => h("button", { class: "chip", "aria-pressed": String(picked.has(f.name)), title: picked.has(f.name) ? `Stop searching ${f.name}` : `Search ${f.name} too`, onclick: () => { if (picked.has(f.name) && picked.size > 1) picked.delete(f.name); else picked.add(f.name); keep(); drawControls(); go(); } }, typeBadge(f), f.name))]);
      clear(meaningBox, vecs.length || !text.length ? null : meaningOffer(t, () => { invalidate(); tables(true).then(() => { mode = "both"; drawControls(); go(); }); }));
    }
    const keep = () => store.set(key, { q: inp.value, mode, fields: [...picked] });
    async function go() {
      const q = inp.value.trim();
      keep();
      if (!q) { clear(out, empty("search", "Type what you are looking for", "A question works: rows with some of its words come back, those with all of them first.")); clear(tail); return; }
      const s = searchSql(t, { q, fields: text.filter((f) => picked.has(f.name)), vec: vecs[0], mode });
      if (!s) return;
      running?.abort();
      const mine = running = new AbortController();
      out.classList.add("loading");
      try {
        const j = await api("/v1/query", { body: { sql: s.sql, params: s.params }, signal: mine.signal });
        if (mine !== running) return;
        last = { s, j, q };
        clear(out, results(t, j, q, s.shown));
        drawTail();
      } catch (e) { if (e.name !== "AbortError") clear(out, h("div", { class: "error" }, e.message)); }
      finally { if (mine === running) { out.classList.remove("loading"); running = null; } }
    }
    function drawTail() {
      if (!last) return;
      const { s, q } = last;
      const callParams = { ...(s.params.q != null ? { q } : {}), ...(s.params.v ? { v: { embed: q } } : {}), k: 10 };
      clear(tail,
        h("div", { class: "row", style: { gap: "8px", flexWrap: "wrap" } },
          gate(h("button", { class: "btn primary", title: "Keep it as a stored function: an endpoint, a line of code, an agent's tool", onclick: async () => { const n = await saveSearch({ suggested: `search_${t.table}`, description: `Finds rows of ${t.table} about a question${s.params.v ? ", by words and meaning" : ""}.`, params: s.fnParams, body: s.body, table: t.table }); if (n) location.hash = `#/saved/${n}`; } }, icon("bookmark"), "Save as a saved search"), "admin"),
          h("a", { class: "btn", href: `#/sql?q=${encodeURIComponent(s.sql.replace(/:q\b/g, `'${q.replace(/'/g, "''")}'`))}${s.params.v ? `&p=${encodeURIComponent(JSON.stringify({ v: { kind: "embed", value: q, field: s.params.v.field } }))}` : ""}` }, icon("sql"), "Open in the workbench"),
          h("button", { class: "btn", onclick: () => copy(s.sql, "SQL copied") }, icon("copy"), "Copy the SQL")),
        h("details", { class: "card-ish" }, h("summary", {}, "The query"), sqlBlock(s.sql), h("div", { class: "small muted", style: { marginTop: "6px" } }, Object.keys(s.params).length ? `Parameters: ${JSON.stringify(s.params)}` : "")),
        h("details", { class: "card-ish" }, h("summary", {}, "Use this search from code"), useFromCode({ sql: s.sql, params: s.params })));
    }
    inp.oninput = debounce(go, 250);
    inp.onkeydown = (e) => { if (e.key === "Enter") go(); };
    pane.append(h("section", { class: "card" }, h("div", { class: "bd stack", style: { gap: "10px" } }, h("div", { class: "row nw search-row", style: { gap: "8px" } }, icon("search"), inp, modeBox), fieldsBox)), meaningBox, out, tail);
    drawControls();
    go();
    setTimeout(() => inp.focus(), 30);
    return () => running?.abort();
  }

  /** The answer of a search as a list: a title, the text with the words marked, the other fields. */
  function results(t, j, q, shown) {
    const rows = j.rows || [], cols = j.columns || [];
    if (!rows.length) return empty("search", "No rows", "Other words, fewer of them, or search by meaning.");
    const words = q.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter((w) => w.length > 2).map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
    const start = words.length ? `(?<![\\p{L}\\p{N}])(${words.join("|")})` : null;
    const ti = shown.title ? cols.indexOf(shown.title) : -1;
    const bi = shown.text ? cols.indexOf(shown.text) : -1;
    const si = cols.indexOf("score()");
    const ki = cols.indexOf(t.short?.key);
    const marked = (s) => {
      s = String(s ?? "");
      if (!words.length) return s;
      const re = new RegExp(start, "giu");
      const out = []; let i = 0;
      for (const m of s.matchAll(re)) { out.push(s.slice(i, m.index), h("mark", {}, m[0])); i = m.index + m[0].length; }
      out.push(s.slice(i));
      return out;
    };
    const snip = (s) => {
      s = String(s ?? "").replace(/\s+/g, " ");
      const at = words.length ? s.toLowerCase().search(new RegExp(start, "u")) : -1;
      const from = at > 80 ? at - 60 : 0;
      return (from ? "…" : "") + s.slice(from, from + 260) + (s.length > from + 260 ? "…" : "");
    };
    return h("section", { class: "card hits-card" }, h("div", { class: "hd" }, h("h3", {}, "Results"), h("span", { class: "sub" }, answerStats(j)), h("div", { class: "tools" }, exportMenu(j, `chloedb-${t.table}`))),
      h("div", { class: "bd flush" }, h("ol", { class: "hits" }, rows.map((r) => h("li", {},
        h("div", { class: "row nw" }, h("b", { class: "grow" }, ti >= 0 ? marked(r[ti]) : ki >= 0 ? String(r[ki]) : "row"), si >= 0 ? h("span", { class: "small muted mono", title: "score" }, Number(r[si]).toFixed(3)) : null),
        bi >= 0 ? h("div", { class: "small dim" }, marked(snip(r[bi]))) : null,
        h("div", { class: "small muted hit-f" }, cols.map((c, i) => i === ti || i === bi || i === si || r[i] == null || String(r[i]).length > 80 ? null : h("span", {}, h("span", { class: "k" }, c), " ", typeof r[i] === "number" ? fmt.num(r[i]) : String(r[i])))))))));
  }

  /** The offer of meaning for a table without vectors: one click, the defaults told. */
  function meaningOffer(t, done) {
    const from = proseFields(t)[0];
    const box = h("section", { class: "card meaning-offer" });
    api("/v1/models").then((j) => {
      const ms = (j.models || []).filter((m) => m.technique === "static");
      const m = ms.find((x) => x.ready && /retrieval/.test(x.name)) || ms.find((x) => x.ready) || ms.find((x) => /retrieval/.test(x.name)) || ms[0];
      if (!m || !from) { box.remove(); return; }
      const taken = new Set((t.short?.fields || []).map((f) => f.name));
      const name = !taken.has("emb") ? "emb" : `${from.name}_emb`;
      const sql = `ALTER TABLE ${quoteName(t.table)} ADD COLUMN ${quoteName(name)} VECTOR${m.dim ? `(${m.dim})` : ""} OPTIONS(embed = '${from.name}', model = '${m.name}')`;
      const btn = gate(h("button", { class: "btn primary" }, icon("sparkle"), "Add meaning"), "admin");
      const status = h("div", { class: "small muted" });
      btn.onclick = async () => {
        btn.disabled = true;
        try {
          await api("/v1/sql", { body: { sql } });
          invalidate();
          for (let i = 0; i < 120 && box.isConnected; i++) {
            const tt = (await tables(true)).find((x) => x.table === t.table);
            const e = (tt?.embeddings || []).find((x) => x.field === name);
            const b = e?.backfill;
            status.textContent = !e ? "Adding the field…" : e.error ? `The model did not open: ${e.error}` : b && b.state !== "done" && b.missing ? `Filling in vectors: ${fmt.n(b.embedded || 0)} done, ${fmt.n(b.missing)} to go…` : e.loaded ? "Ready" : "Opening the model…";
            if (e?.error) break;
            if (e?.loaded && (!b || b.state === "done" || !b.missing)) { toast(`${t.table} searches by meaning now`, "ok"); done(); return; }
            await new Promise((res) => setTimeout(res, 1500));
          }
        } catch (e) { toast(e.message, "bad"); btn.disabled = false; }
      };
      clear(box, h("div", { class: "bd row", style: { gap: "14px", alignItems: "flex-start" } }, icon("sparkle"),
        h("div", { class: "grow stack", style: { gap: "4px" } }, h("b", {}, "Search by meaning too"),
          h("div", { class: "small dim" }, `Finds rows that say the same thing in other words. One click adds a vector field ${name}, embedded from ${from.name} by ${m.name} (${m.dim || "?"} dimensions, a static model: well under a millisecond a text on a CPU${m.ready ? "" : "; the server fetches it first"}). The rows already there get their vectors in the background.`),
          h("details", { class: "small" }, h("summary", { class: "muted" }, "The statement, and other choices"), sqlBlock(sql), h("div", { style: { marginTop: "6px" } }, "Another model, index or coding: ", h("a", { href: "#/models" }, "Models"), ".")), status),
        btn));
    }).catch(() => box.remove());
    return box;
  }

  // ---------------------------------------------------------- Data
  // The rows, a page at a time: a condition to filter by (the WHERE of a
  // statement), a field to sort on, a page size, and paging. A row opens
  // with every field in full. Vectors, and fields made from others and
  // not kept (a generated TEXT), are left out: they hold nothing to read.
  function dataTab(pane, t) {
    const all = t.short?.fields || [];
    const shown = all.filter((f) => !["vector", "sparse", "edges"].includes(f.type) && !f.from);
    const left = all.filter((f) => !shown.includes(f));
    const sortable = shown.filter((f) => !["text"].includes(f.type) && !f.multi);
    const key = t.short?.key;
    const tq = quoteName(t.table);
    const saved = store.get(`table.data.${t.table}`, {}) || {};
    let where = saved.where || "", by = sortable.some((f) => f.name === saved.by) ? saved.by : key || sortable[0]?.name || "", desc = !!saved.desc, size = [25, 50, 100].includes(saved.size) ? saved.size : 50, page = 0, total = null, seq = 0, last = null;
    const keep = () => store.set(`table.data.${t.table}`, { where, by, desc, size });
    const sqlOf = (n = size, off = page * size) => `SELECT ${shown.map((f) => quoteName(f.name)).join(", ") || "*"} FROM ${tq}${where.trim() ? ` WHERE ${where.trim()}` : ""}${by ? ` ORDER BY ${quoteName(by)}${desc ? " DESC" : ""}` : ""} LIMIT ${n}${off ? ` OFFSET ${off}` : ""}`;
    const filter = h("input", { class: "input mono grow", value: where, placeholder: sortable.length ? `a condition, e.g. ${sortable.find((f) => f.type === "keyword")?.name || sortable[0].name} = '…'${sortable.find((f) => /int|float/.test(f.type)) ? ` AND ${sortable.find((f) => /int|float/.test(f.type)).name} > 10` : ""}` : "a condition, as after WHERE", "aria-label": "Filter: a condition, as after WHERE", spellcheck: "false", autocomplete: "off" });
    const sortSel = h("select", { class: "input", "aria-label": "Sort by", title: "Sort by" }, h("option", { value: "" }, "no order"), sortable.map((f) => h("option", { value: f.name, selected: f.name === by }, f.name)));
    const dirBtn = h("button", { class: "btn sm", title: "Ascending or descending" });
    const sizeSeg = seg([[25, "25"], [50, "50"], [100, "100"]], size, (v) => { size = v; page = 0; keep(); load(); }, { "aria-label": "Rows a page" });
    const prev = h("button", { class: "btn sm", title: "The page before" }, icon("left"), "Prev");
    const next = h("button", { class: "btn sm", title: "The page after" }, "Next", icon("right"));
    const count = h("span", { class: "small muted nowrap" });
    const err = h("div", { class: "error small", hidden: true });
    const out = h("div", { class: "small muted", style: { padding: "12px" } }, "Reading rows…");
    const wb = h("a", { class: "btn sm" }, icon("sql"), "Open in the workbench");
    const exp = h("span", { class: "row nw", style: { gap: "4px" } });
    const drawDir = () => clear(dirBtn, icon(desc ? "down" : "up"), desc ? "Desc" : "Asc");
    drawDir();
    filter.onkeydown = (e) => { if (e.key === "Enter") { where = filter.value; page = 0; keep(); load(); } };
    filter.onchange = () => { if (filter.value !== where) { where = filter.value; page = 0; keep(); load(); } };
    sortSel.onchange = () => { by = sortSel.value; page = 0; keep(); load(); };
    dirBtn.onclick = () => { desc = !desc; drawDir(); page = 0; keep(); load(); };
    prev.onclick = () => { if (page > 0) { page--; load(false); } };
    next.onclick = () => { page++; load(false); };
    pane.append(h("section", { class: "card" },
      h("div", { class: "hd" }, h("h3", {}, "Rows"), h("span", { class: "sub" }, `click a row for every field in full${left.length ? ` · not shown: ${left.map((f) => `${f.name} (${f.from ? `made from ${f.from.join(", ")}` : f.type})`).join(", ")}` : ""}`),
        h("div", { class: "tools" }, exp, wb, gate(h("a", { class: "btn sm", href: "#/import" }, icon("import"), "Add rows"), "write"))),
      h("div", { class: "bd stack", style: { gap: "8px", paddingBottom: "8px" } },
        h("div", { class: "row data-tools", style: { gap: "8px", flexWrap: "wrap" } }, h("span", { class: "small muted mono" }, "WHERE"), filter, h("span", { class: "small muted" }, "sort"), sortSel, dirBtn, sizeSeg),
        err),
      h("div", { class: "bd flush" }, out),
      h("div", { class: "ft row", style: { gap: "8px", padding: "8px 14px" } }, count, h("span", { class: "sep grow" }), prev, next)));
    async function load(recount = true) {
      const my = ++seq;
      const sql = sqlOf();
      wb.href = `#/sql?q=${encodeURIComponent(sqlOf(size, page * size))}`;
      prev.disabled = page === 0;
      out.style.opacity = ".55";
      try {
        const [j, c] = await Promise.all([api("/v1/query", { body: { sql } }), recount || total == null ? api("/v1/query", { body: { sql: `SELECT COUNT(*) FROM ${tq}${where.trim() ? ` WHERE ${where.trim()}` : ""}` } }).then((x) => x.rows?.[0]?.[0] ?? null).catch(() => null) : Promise.resolve(total)]);
        if (my !== seq) return;
        total = c; last = j;
        err.hidden = true;
        const n = j.rows?.length || 0;
        const from = page * size + (n ? 1 : 0), to = page * size + n;
        count.textContent = n ? `rows ${fmt.n(from)}–${fmt.n(to)}${total != null ? ` of ${fmt.n(total)}${where.trim() ? " matching" : ""}` : ""}` : where.trim() ? "no row matches" : "";
        next.disabled = n < size || (total != null && to >= total);
        clear(exp, n ? exportMenu(j, t.table) : null);
        clear(out, n ? h("div", { class: "tbl-wrap" }, grid(j, { max: size, first: page * size + 1 })) : where.trim() ? empty("search", "No row matches", `Nothing in ${t.table} has ${where.trim()}.`, h("button", { class: "btn", onclick: () => { filter.value = where = ""; page = 0; keep(); load(); } }, "Clear the filter")) : empty("table", "No rows yet", "Import a file into it, or write rows from code.", gate(h("a", { class: "btn", href: "#/import" }, icon("import"), "Import into it"), "write")));
      } catch (e) {
        if (my !== seq) return;
        // A refused condition or a server that does not answer leaves no
        // page: not the last one's rows and count beside the error, nor a
        // Next to a page that cannot be read (CCB-19).
        failed = !(e instanceof ApiError && e.answered);
        last = null; total = null;
        err.hidden = false;
        clear(err, icon("warn"), " ", e.message, where.trim() && !failed ? h("span", { class: "muted" }, " · a condition is what follows WHERE: brand = 'x' AND price > 10") : null);
        count.textContent = "";
        next.disabled = true;
        clear(exp);
        clear(out, failed ? h("div", { class: "small muted", style: { padding: "12px" } }, "The rows show again once the server answers.") : null);
      } finally { if (my === seq) out.style.opacity = ""; }
    }
    // A page the server did not answer is read again when it answers.
    let failed = false;
    const off = bus.on("connection", (c) => { if (c.ok && failed) { failed = false; load(); } });
    load();
    return off;
  }

  // ---------------------------------------------------------- Quality
  // Per field: how filled it is, and what is in it. Numbers and times
  // their range and mean; keywords how many values and the commonest;
  // text how long, and how many are empty strings; vectors how many rows
  // have one. A field made from others is not kept, so it has no count.
  function qualityTab(pane, t) {
    const all = t.short?.fields || [];
    const fs = all.filter((f) => !["edges", "vector", "sparse"].includes(f.type) && !f.from);
    const made = all.filter((f) => f.from && !["vector", "sparse"].includes(f.type));
    const out = h("div", { class: "small muted", style: { padding: "12px" } }, "Counting…");
    pane.append(h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Each field"), h("span", { class: "sub" }, "how filled it is, and what is in it")), h("div", { class: "bd" }, out)));
    (async () => {
      const tq = quoteName(t.table);
      const n = (await api("/v1/query", { body: { sql: `SELECT COUNT(*) FROM ${tq}` } })).rows?.[0]?.[0] || 0;
      if (!n) { clear(out, empty("table", "No rows to judge", "Quality shows once rows are in.")); return; }
      const stats = await profile(t, fs, n);
      const notes = [];
      const num = (v, f) => v == null ? "–" : /date|time/.test(f.type) && typeof v === "number" && v > 1e11 ? new Date(v).toISOString().slice(0, 10) : typeof v === "string" && /^\d{4}-\d\d-\d\dT/.test(v) ? v.replace(/T00:00:00(\.0+)?Z$/, "").replace("T", " ").replace(/(\.\d+)?Z$/, "") : typeof v === "number" ? (Number.isInteger(v) ? fmt.n(v) : fmt.n(Math.round(v * 100) / 100)) : String(v);
      const rowsEl = fs.map((f, i) => {
        const { filled: c, more } = stats[i];
        const share = c == null ? null : c / n;
        if (share != null && share < 0.5 && f.name !== t.short?.key) notes.push(`${f.name} is empty in ${fmt.pct(1 - share, 0)} of the rows`);
        let what = "";
        if (/^(int|float|date|time|timestamp)/.test(f.type) && more?.[0]) { const [mn, mx, av] = more[0]; what = `${num(mn, f)} to ${num(mx, f)}${av != null && f.name !== t.short?.key ? ` · mean ${num(av, f)}` : ""}`; if (mn === mx && n > 20) notes.push(`${f.name} is ${num(mn, f)} in every row`); }
        else if (f.type === "keyword") {
          const [d, top] = f.multi ? [null, more] : more || [];
          const k = d?.[0]?.[0];
          if (top?.length && top[0][1] / n > 0.9 && n > 20 && !f.multi) notes.push(`${f.name} holds one value (${top[0][0]}) in ${fmt.pct(top[0][1] / n, 0)} of the rows`);
          if (k != null && k === n && f.name !== t.short?.key && n > 20) notes.push(`${f.name} is different in every row: an id, not a category to filter by`);
          what = [k != null ? `${fmt.n(k)} values` : f.multi ? "a list a row" : null, top?.length ? "most: " + top.map((x) => `${x[0] ?? "null"} (${fmt.n(x[1])})`).join(", ") : null].filter(Boolean).join(" · ");
        } else if (f.type === "text" && more) {
          const [len, emptyN] = more; const av = len?.[0]?.[0], mx = len?.[0]?.[1], e = emptyN?.[0]?.[0];
          if (e) notes.push(`${f.name} is an empty string in ${fmt.plural(e, "row")}`);
          what = [av != null ? `about ${fmt.n(Math.round(av))} characters, at most ${fmt.n(mx)}` : null, e ? `${fmt.n(e)} empty` : null].filter(Boolean).join(" · ");
        }
        return h("div", { class: "q-row" }, h("span", { class: "row nw" }, typeBadge(f), h("b", { class: "mono" }, f.name)),
          h("span", { class: "q-bar", title: share == null ? "could not count" : `${fmt.n(c)} of ${fmt.n(n)}` }, h("i", { style: { width: share == null ? "0" : Math.max(1, share * 100) + "%", background: share != null && share < 0.5 ? "var(--serious)" : "var(--accent)" } })),
          h("span", { class: "small mono" }, share == null ? "–" : fmt.pct(share, share < 1 && share > 0.99 ? 1 : 0)),
          h("span", { class: "small muted ellipsis", title: what }, what));
      });
      const vecs = (t.embeddings || []).map((e) => {
        // A backfill that is done has embedded what it found missing (CCB-4).
        const b = e.backfill, miss = b?.state === "done" ? Math.max(0, (b.missing || 0) - (b.embedded || 0)) : b?.missing || 0;
        if (miss) notes.push(`${e.field} has no vector yet for ${fmt.n(miss)} rows (being filled in)`);
        if (e.error) notes.push(`${e.field}: the model did not open (${e.error})`);
        const share = miss ? Math.max(0, 1 - miss / n) : 1;
        return h("div", { class: "q-row" }, h("span", { class: "row nw" }, typeBadge({ type: "vector" }), h("b", { class: "mono" }, e.field)),
          h("span", { class: "q-bar", title: miss ? `${fmt.n(miss)} rows to go` : "every row has a vector" }, h("i", { style: { width: share * 100 + "%", background: miss ? "var(--serious)" : "var(--accent)" } })),
          h("span", { class: "small mono" }, fmt.pct(share, 0)), h("span", { class: "small muted ellipsis" }, `by ${e.model}, from ${(e.from || []).join(", ")}`));
      });
      for (const f of made) notes.push(`${f.name} is made from ${f.from.join(", ")} and not kept: it is searched, not read`);
      if (!proseFields(t).length) notes.push("No text field to search: words search needs a TEXT field");
      const worry = notes.filter((x) => !/ is made from /.test(x));
      clear(out, h("div", { class: "row small", style: { gap: "16px", marginBottom: "10px", flexWrap: "wrap" } }, h("span", {}, h("b", {}, fmt.n(n)), " rows"), h("span", {}, h("b", {}, fmt.n(fs.length + vecs.length)), " fields checked"), h("span", {}, worry.length ? h("b", { style: { color: "var(--serious)" } }, fmt.plural(worry.length, "thing")) : h("b", {}, "nothing"), " to look at")),
        notes.length ? h("ul", { class: "q-notes" }, notes.map((x) => h("li", {}, icon(/ is made from /.test(x) ? "info" : "warn"), " ", x))) : h("p", { class: "small", style: { marginTop: 0 } }, icon("okc"), " Every field is set in most rows."),
        h("div", { class: "q-rows" }, rowsEl, vecs));
    })().catch((e) => clear(out, h("div", { class: "error" }, e.message)));
  }

  // ---------------------------------------------------------- Used by
  function usedTab(pane, t) {
    const out = h("div", { class: "stack" }, h("div", { class: "small muted", style: { padding: "12px" } }, "Looking…"));
    pane.append(out);
    const re = new RegExp(`(^|[^a-z0-9_])"?${t.table.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}"?([^a-z0-9_]|$)`, "i");
    Promise.allSettled([api("/v1/functions"), can("admin") ? api("/v1/admin/keys") : Promise.reject(new Error("needs an admin key")), api("/v1/queries/stats?limit=500")]).then(([fn, ks, st]) => {
      const fns = fn.status === "fulfilled" ? (fn.value.functions || []).filter((f) => f.current != null && f.versions.some((v) => v.version === f.current && re.test(v.body || ""))) : [];
      const keys = ks.status === "fulfilled" ? (ks.value.keys || []).filter((k) => !k.revoked_ms && k.tables && Object.keys(k.tables).some((p) => p === "*" || (p.endsWith("*") ? t.table.startsWith(p.slice(0, -1)) : p === t.table))) : null;
      const named = st.status === "fulfilled" ? (Array.isArray(st.value) ? st.value : st.value.statements || []).filter((s) => re.test(s.sql || s.example || s.fingerprint || "")) : [];
      // The command center's own reads (this page's counts, the Data tab) are not a use of the table.
      const ours = (s) => s.command_center && Object.keys(s.clients || {}).every((c) => c === "command-center");
      const sts = named.filter((s) => !ours(s)), mineN = named.length - sts.length;
      const byKey = new Map();
      for (const s of sts) for (const k of s.keys || []) if (k.key) byKey.set(k.key, (byKey.get(k.key) || 0) + (k.calls || 0));
      const o = ownerOf(t.table);
      clear(out,
        o ? card({ title: "App", body: h("div", {}, h("b", {}, o.owner), h("span", { class: "dim" }, `: ${o.about}. Its writes come from the app, not from people.`)) }) : null,
        card({ title: "Saved searches", sub: `${fns.length} call this table`, body: fns.length ? h("div", { class: "row", style: { gap: "6px", flexWrap: "wrap" } }, fns.map((f) => h("a", { class: "chip", href: `#/saved/${encodeURIComponent(f.name)}` }, icon("bookmark"), f.name))) : h("div", { class: "small muted" }, "None yet. ", h("a", { href: `#/table/${encodeURIComponent(t.table)}?tab=search` }, "Search it"), ", then save the search.") }),
        card({ title: "Keys limited to it", sub: keys == null ? "needs an admin key" : `${keys.length} keys`, body: keys == null ? h("div", { class: "small muted" }, "The key list needs an admin key; below are the keys seen calling it.") : keys.length ? h("div", { class: "stack", style: { gap: "4px" } }, keys.map((k) => h("div", { class: "row nw small" }, icon("key"), h("b", {}, k.name), h("span", { class: "muted" }, ` ${k.scope}${k.owner ? " · owner " + k.owner : ""} · ${k.last_used_ms ? "used " + fmt.ago(k.last_used_ms) : "never used"}`)))) : h("div", { class: "small muted" }, "No key is limited to this table; keys without limits read every table.") }),
        card({ title: "Who queried it lately", sub: `the statements of the last 15 minutes that name it${mineN ? `, besides ${fmt.plural(mineN, "statement")} of the command center itself` : ""}`, flush: !!sts.length, body: sts.length ? h("div", { class: "stack", style: { gap: "0" } },
          byKey.size ? h("div", { class: "row small", style: { gap: "6px", padding: "10px 14px", flexWrap: "wrap" } }, "Keys: ", [...byKey].sort((a, b) => b[1] - a[1]).map(([k, n]) => h("a", { class: "chip", href: `#/queries?key=${encodeURIComponent(k)}` }, icon("key"), `${k} · ${fmt.plural(n, "call")}`))) : null,
          sts.slice(0, 12).map((s) => h("a", { class: "used-st", href: `#/statement/${encodeURIComponent(s.fingerprint || s.sql)}` }, h("span", { class: "mono small ellipsis grow" }, s.sql || s.example || s.fingerprint), h("span", { class: "small muted" }, `${fmt.plural(s.calls || 0, "call")}${s.p99_ms != null ? " · p99 " + fmt.ms(s.p99_ms) : ""}`)))) : h("div", { class: "small muted" }, "No statement named it in the last 15 minutes.") }));
    });
  }

  // ---------------------------------------------------------- Storage
  function storageTab(pane, t) {
    const host = h("div");
    pane.append(host);
    try {
      const c = inspector.render(host, { id: "inspector", rest: [], params: new URLSearchParams({ table: t.table }), embedded: true });
      return typeof c === "function" ? c : null;
    } catch (e) { clear(host, failure(e, "The storage")); return null; }
  }

  show(r);
  return () => { alive = false; again = null; stop?.(); };
}
