// Library (SPEC.md, "Fourth build", rows 1 and 10): what the agents search.
// Code: the repository's files as chunks (a function, a class, a heading,
// else 60 lines) in Workbench's own ChloeDB, ranked by BM25 with
// identifiers split (`cartTotal` finds `cart_total`), as an agent's
// worktree sees them (its edits overlaid), and, with a token budget, the
// one `PACK n TOKENS` text an agent gets from search_code. Docs: the
// repository's dependencies, their doc sites crawled into rows, and which
// agent's search_docs answer cited which page.
//   GET /api/search/code, /api/search/index, POST /api/search/reindex,
//   GET /api/library, /api/library/:id/pages, POST /api/library/crawl,
//   GET /api/search/docs
import {
  h, useStyle, avatar, ctxChip, pill, empty, fmtAgo, fmtNum, fmtTokens, repoOf, later, keyNav, setFleet, toMs,
} from "./common.js";
// Sixth build: a repository read without an agent (ui/levels/repos.js).
import { repoPicker, repoSwitch } from "./repos.js";

/** What the quick search asks the screen to open with: {tab, q, focus}. */
export const intent = {};
export function openWith(o) { Object.assign(intent, o); }

const nameOf = (a) => (a && (a.name || a.id)) || "?";
const LANG_COLOR = { rust: "var(--lane-2)", typescript: "var(--lane-1)", javascript: "var(--warn)", python: "var(--lane-3)", go: "var(--lane-5)", markdown: "var(--lane-4)", sql: "var(--muted)", other: "var(--line-2)" };
const langColor = (l) => LANG_COLOR[l] || "var(--line-2)";
const ECO = { crates: "crates.io", npm: "npm", pypi: "PyPI", go: "Go", site: "site" };
const BUDGETS = [1000, 2000, 4000, 8000];

/** The words of a query as the index splits them: camelCase and snake_case apart. */
export function queryWords(q) {
  return String(q || "").replace(/([a-z0-9])([A-Z])/g, "$1 $2").replace(/([A-Z]+)([A-Z][a-z])/g, "$1 $2").toLowerCase().split(/[^a-z0-9]+/).filter((w) => w.length > 1);
}

const svg = (d, cls = "i") => {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("class", cls);
  s.setAttribute("aria-hidden", "true");
  s.innerHTML = d;
  return s;
};
export const ICON = {
  search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-4-4"/>',
  code: '<path d="M8 7l-5 5 5 5M16 7l5 5-5 5"/>',
  doc: '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5M9 13h6M9 17h4"/>',
  refresh: '<path d="M20 11a8 8 0 0 0-14.3-4.5L4 8.5"/><path d="M4 4v4.5h4.5"/><path d="M4 13a8 8 0 0 0 14.3 4.5l1.7-2"/><path d="M20 20v-4.5h-4.5"/>',
  copy: '<rect x="8.5" y="8.5" width="11.5" height="11.5" rx="2"/><path d="M15.5 8.5V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v7.5a2 2 0 0 0 2 2h2.5"/>',
  link: '<path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1"/><path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/>',
  pack: '<path d="M3 7l9-4 9 4-9 4z"/><path d="M3 7v10l9 4 9-4V7M12 11v10"/>',
  edit: '<path d="M4 20h4L19 9l-4-4L4 16z"/><path d="M13.5 6.5l4 4"/>',
  globe: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c3 3.5 3 14.5 0 18M12 3c-3 3.5-3 14.5 0 18"/>',
};
export const icon = (name, cls) => svg(ICON[name], cls);

// ------------------------------------------------------------ code tinting

const KW = new Set(("fn pub let mut const static struct enum impl trait use mod match if else for while loop return async await move where type as in ref self Self crate super " +
  "function export import from default class interface extends implements new this typeof instanceof var of case switch break continue throw try catch finally yield " +
  "def lambda with pass raise None True False and or not is elif nonlocal global func package go defer chan map range struct select").split(" "));
const TOKEN = /(\/\/.*$|#(?![\[!]).*$|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.){2,}'|`(?:[^`\\]|\\.)*`|\b\d[\d_.]*\b|\b[A-Za-z_][A-Za-z0-9_]*\b)/g;

/** A line of code as nodes: keywords, strings, comments, numbers tinted, the query's words marked. */
export function tintLine(line, lang, marks) {
  const out = [];
  const comment = lang === "python" || lang === "shell" || lang === "toml" || lang === "yaml" ? /^#/ : /^\/\//;
  if (lang === "markdown") return markWords(line, marks, line.startsWith("#") ? "lb-t-kw" : null);
  let at = 0, m;
  TOKEN.lastIndex = 0;
  while ((m = TOKEN.exec(line))) {
    if (m.index > at) out.push(...markWords(line.slice(at, m.index), marks));
    const t = m[0];
    let cls = null;
    if (t.startsWith("//") || t.startsWith("#")) cls = comment.test(t) ? "lb-t-com" : null;
    else if (/^["'`]/.test(t)) cls = "lb-t-str";
    else if (/^\d/.test(t)) cls = "lb-t-num";
    else if (KW.has(t)) cls = "lb-t-kw";
    else if (/^[A-Z]/.test(t)) cls = "lb-t-ty";
    if (cls === null && (t.startsWith("//") || t.startsWith("#"))) { out.push(...markWords(t, marks)); at = TOKEN.lastIndex; continue; }
    out.push(...markWords(t, marks, cls));
    at = TOKEN.lastIndex;
  }
  if (at < line.length) out.push(...markWords(line.slice(at), marks));
  return out;
}

function markWords(text, marks, cls) {
  const wrap = (s) => (cls ? h("span", { class: cls }, s) : s);
  if (!marks || !text) return [wrap(text)];
  const out = [];
  let at = 0, m;
  marks.lastIndex = 0;
  while ((m = marks.exec(text))) {
    if (!m[0]) { marks.lastIndex++; continue; }
    if (m.index > at) out.push(wrap(text.slice(at, m.index)));
    out.push(h("mark", { class: cls || null }, m[0]));
    at = marks.lastIndex;
  }
  if (at < text.length) out.push(wrap(text.slice(at)));
  return out;
}

export function marksOf(q) {
  const words = queryWords(q).sort((a, b) => b.length - a.length);
  if (!words.length) return null;
  return new RegExp(words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|"), "gi");
}

/** path with its last part bold. */
export function pathNode(p) {
  const i = p.lastIndexOf("/");
  return h("span", { class: "lb-path" }, i >= 0 ? h("span", { class: "lb-dir" }, p.slice(0, i + 1)) : null, h("b", null, p.slice(i + 1)));
}

const count = (n) => (n == null ? "" : Number(n).toLocaleString("en-US"));
const range = (c) => (c.end_line > c.start_line ? `${c.start_line}–${c.end_line}` : `${c.start_line}`);
const hostOf = (u) => { try { return new URL(u).host; } catch { return u; } };
const pathOfUrl = (u) => { try { const x = new URL(u); return x.pathname + x.search; } catch { return u; } };

export function mount(root, ctx) {
  const qs = new URLSearchParams(location.search);
  const hashTab = (location.hash.match(/^#\/library\/(code|docs)/) || [])[1];
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem("wb-library") || "{}"); } catch {}
  const S = {
    tab: intent.tab || hashTab || qs.get("tab") || saved.tab || "code",
    q: intent.q ?? saved.q ?? "", view: "", lang: "", budget: saved.budget || 0,
    code: null, codeErr: null, codeBusy: false, index: null, indexErr: null, reindexing: false, focus: intent.focus || null, open: new Set(),
    lib: null, libSel: intent.library || saved.lib || null, pages: null, pagesErr: null, dq: intent.tab === "docs" ? intent.q || "" : "", docs: null, crawlBusy: new Set(), addUrl: "", packOpen: false, copied: null,
  };
  for (const k of Object.keys(intent)) delete intent[k];
  const el = h("div", { class: "lv-screen lb" });
  root.append(el);
  let alive = true, poll = null;
  const repo = () => repoOf(ctx);
  const save = () => { try { localStorage.setItem("wb-library", JSON.stringify({ tab: S.tab, q: S.q, budget: S.budget, lib: S.libSel })); } catch {} };

  // ---------------------------------------------------------------- data
  async function loadIndex() {
    try {
      S.index = await ctx.api(`/api/search/index?${new URLSearchParams({ repo: repo() })}`);
      S.indexErr = null;
      setFleet([...(ctx.state?.agents || []), ...(S.index.agents || [])].filter((a, i, all) => all.findIndex((b) => b.id === a.id) === i));
    } catch (e) { S.indexErr = e.message || String(e); }
    if (alive) render();
  }
  let codeReq = 0;
  async function search() {
    const q = S.q.trim();
    save();
    if (!q) { S.code = null; S.codeBusy = false; render(); return; }
    const req = ++codeReq;
    S.codeBusy = true;
    render();
    const p = new URLSearchParams({ repo: repo(), q, limit: "20" });
    if (S.view) p.set("agent", S.view);
    if (S.lang) p.set("lang", S.lang);
    if (S.budget) p.set("tokens", String(S.budget));
    try {
      const r = await ctx.api(`/api/search/code?${p}`);
      if (req !== codeReq) return;
      S.code = r; S.codeErr = null;
      if (r.index) S.index = { ...(S.index || {}), index: r.index };
    } catch (e) { if (req === codeReq) S.codeErr = e.message || String(e); }
    if (req === codeReq) S.codeBusy = false;
    if (alive) render();
  }
  const searchSoon = later(search, 180);

  async function reindex() {
    S.reindexing = true; render();
    try { await ctx.api("/api/search/reindex", { method: "POST", body: { repo: repo() } }); } catch (e) { S.indexErr = e.message || String(e); }
    S.reindexing = false;
    await loadIndex();
    if (S.q) search();
  }

  async function loadLibs() {
    try {
      S.lib = await ctx.api(`/api/library?${new URLSearchParams({ repo: repo() })}`);
      S.libErr = null;
      const libs = S.lib.libraries || [];
      if (!S.libSel || !libs.some((l) => l.id === S.libSel)) S.libSel = (libs.find((l) => l.state === "done") || libs[0] || {}).id || null;
    } catch (e) { S.libErr = e.message || String(e); }
    if (alive) { render(); schedulePoll(); }
    if (S.libSel && !S.pages) loadPages();
  }
  async function loadPages() {
    const id = S.libSel;
    if (!id) return;
    try {
      const r = await ctx.api(`/api/library/${encodeURIComponent(id)}/pages?${new URLSearchParams({ repo: repo() })}`);
      if (id === S.libSel) { S.pages = r; S.pagesErr = null; }
    } catch (e) { S.pagesErr = e.message || String(e); }
    if (alive) render();
  }
  function schedulePoll() {
    clearTimeout(poll);
    const busy = (S.lib?.libraries || []).some((l) => l.state === "crawling" || l.state === "queued");
    if (busy && alive) poll = setTimeout(async () => { await loadLibs(); if (S.libSel) loadPages(); }, 2000);
  }
  async function crawl(body, key) {
    S.crawlBusy.add(key); render();
    try { await ctx.api("/api/library/crawl", { method: "POST", body: { repo: repo(), ...body } }); S.libErr = null; }
    catch (e) { S.libErr = e.message || String(e); }
    S.crawlBusy.delete(key);
    await loadLibs();
  }
  let docReq = 0;
  async function searchDocs() {
    const q = S.dq.trim();
    if (!q) { S.docs = null; render(); return; }
    const req = ++docReq;
    const p = new URLSearchParams({ repo: repo(), q, limit: "12" });
    if (S.docScope === "lib" && S.libSel) p.set("library", S.libSel);
    try { const r = await ctx.api(`/api/search/docs?${p}`); if (req === docReq) { S.docs = r; S.docsErr = null; } }
    catch (e) { if (req === docReq) S.docsErr = e.message || String(e); }
    if (alive) render();
  }
  const searchDocsSoon = later(searchDocs, 200);

  function setTab(t) {
    if (S.tab === t) return;
    S.tab = t; save();
    try { if (location.hash.startsWith("#/library")) history.replaceState(history.state, "", `#/library/${t}`); } catch {}
    if (t === "docs" && !S.lib) loadLibs();
    if (t === "code" && !S.index) loadIndex();
    render();
    requestAnimationFrame(() => el.querySelector(t === "code" ? ".lb-q input" : ".lb-dq input")?.focus({ preventScroll: true }));
  }

  // --------------------------------------------------------------- render
  function render() {
    const keep = document.activeElement && el.contains(document.activeElement) ? document.activeElement : null;
    const keepKey = keep?.dataset?.key, selStart = keep?.selectionStart, selEnd = keep?.selectionEnd;
    const ix = S.code?.index || S.index?.index;
    const nLibs = S.lib?.libraries?.length;
    const head = h("header", { class: "lv-head lb-head" },
      h("h1", { class: "lv-h1" }, "Library"),
      h("span", { class: "lv-sub" }, "code and docs your agents search · BM25 in Workbench's own ChloeDB, nothing sent to a model"),
      h("div", { class: "lb-tabs", role: "tablist", "aria-label": "Library" },
        tabBtn("code", "Code", ix ? `${count(ix.chunks)} chunks` : null),
        tabBtn("docs", "Docs", nLibs != null ? `${nLibs} libraries` : null)),
      repo() ? repoSwitch(ctx, repo()) : null,
    );
    if (!repo()) { if (el.querySelector(".rp")) return; el.replaceChildren(head, repoPicker(ctx, { focus: !keepKey, why: "Library searches a repository's code (and the docs of its libraries) in Workbench's own ChloeDB. No agent is needed: open a git checkout and it is indexed now." })); return; }
    el.replaceChildren(head, S.tab === "code" ? codeTab() : docsTab());
    if (keepKey) {
      const k = el.querySelector(`[data-key="${CSS.escape(keepKey)}"]`);
      if (k) { k.focus({ preventScroll: true }); if (selStart != null && k.setSelectionRange) try { k.setSelectionRange(selStart, selEnd); } catch {} }
    }
  }
  function tabBtn(t, label, n) {
    return h("button", { role: "tab", "aria-selected": String(S.tab === t), class: S.tab === t ? "on" : "", "data-key": "tab-" + t, onclick: () => setTab(t) },
      icon(t === "code" ? "code" : "doc", "i sm"), label, n ? h("span", { class: "lb-tab-n" }, n) : null);
  }

  // ------------------------------------------------------------------ code
  function codeTab() {
    const ix = S.code?.index || S.index?.index;
    const overlays = ix?.overlays || [];
    const input = h("input", {
      type: "search", value: S.q, placeholder: "Search code: a symbol, a few words, an error…", onfocus: (e) => import("./complete.js").then((m) => m.attachComplete(e.target, { field: "search", ctx })).catch(() => {}), "data-key": "q", "aria-label": "Search code", spellcheck: "false", autocomplete: "off",
      oninput: (e) => { S.q = e.target.value; searchSoon(); },
      onkeydown: (e) => { if (e.key === "Enter") { searchSoon.cancel(); search(); } if (e.key === "ArrowDown") { e.preventDefault(); el.querySelector(".lb-hit")?.focus(); } },
    });
    const bar = h("div", { class: "lb-bar" },
      h("label", { class: "lb-q" }, icon("search"), input, S.codeBusy ? h("span", { class: "spin" }) : h("kbd", null, "/")),
      h("label", { class: "lb-sel", title: "Whose files: the main checkout, or an agent's worktree with its edits" },
        h("span", null, "as seen by"),
        h("select", { "data-key": "view", onchange: (e) => { S.view = e.target.value; search(); } },
          h("option", { value: "", selected: !S.view }, "the main checkout"),
          overlays.map((o) => h("option", { value: o.agent.id, selected: S.view === o.agent.id }, `${nameOf(o.agent)}'s worktree`)))),
      h("div", { class: "lb-pack-ctl", role: "group", "aria-label": "What an agent gets" },
        h("button", { class: "lb-pack-btn" + (S.budget ? " on" : ""), "aria-pressed": String(!!S.budget), title: "Show the one text search_code answers an agent with, packed to a token budget (PACK n TOKENS)", onclick: () => { S.budget = S.budget ? 0 : 2000; save(); search(); } }, icon("pack", "i sm"), "Pack"),
        S.budget ? h("select", { "aria-label": "Token budget", onchange: (e) => { S.budget = Number(e.target.value); save(); search(); } },
          BUDGETS.map((b) => h("option", { value: b, selected: S.budget === b }, `${fmtTokens(b)} tokens`))) : null));
    const langs = (ix?.langs || []).filter((l) => l.chunks > 0);
    const filters = langs.length ? h("div", { class: "lb-langs" },
      h("button", { class: "lb-lang" + (!S.lang ? " on" : ""), onclick: () => { S.lang = ""; search(); } }, "All languages"),
      langs.slice(0, 7).map((l) => h("button", { class: "lb-lang" + (S.lang === l.lang ? " on" : ""), onclick: () => { S.lang = S.lang === l.lang ? "" : l.lang; search(); } },
        h("i", { style: { background: langColor(l.lang) } }), l.lang, h("span", null, fmtNum(l.files))))) : null;
    return h("div", { class: "lb-grid" },
      h("div", { class: "lb-main" }, bar, filters, results()),
      h("aside", { class: "lb-side" }, indexCard(), freshCard(), searchesCard()));
  }

  function results() {
    if (S.codeErr) return h("div", { class: "err" }, `The search failed: ${S.codeErr}`);
    const ix = S.code?.index || S.index?.index;
    if (ix && ix.state === "indexing") {
      const p = ix.progress || {};
      return h("section", { class: "lv-card lb-indexing" }, h("span", { class: "spin" }), h("div", null, h("b", null, "Indexing the repository"),
        h("div", { class: "lv-sub" }, p.total ? `${fmtNum(p.done)} of ${fmtNum(p.total)} files read` : "listing its files…")));
    }
    if (!S.q.trim()) return startHere();
    const r = S.code;
    if (!r) return h("div", { class: "lb-meta" }, "Searching…");
    const words = queryWords(S.q);
    const split = words.length > 1 && /[a-z][A-Z]|_/.test(S.q) ? [" · identifiers split: ", h("code", { class: "lv-code" }, S.q.trim()), " → ", h("code", { class: "lv-code" }, words.join(" "))] : [];
    const meta = h("div", { class: "lb-meta" },
      h("b", null, `${r.hits.length} chunk${r.hits.length === 1 ? "" : "s"}`), ` · ${r.took_ms ?? 0} ms · BM25 over code_chunks`, ...split,
      r.agent ? [" · ", avatar(r.agent, 14), ` ${nameOf(r.agent)}'s worktree overlaid`] : null);
    const marks = marksOf(S.q);
    return h("div", { class: "lb-results" }, meta,
      (r.missing || []).map((m) => h("div", { class: "lb-missing" }, m)),
      r.pack ? packCard(r.pack) : null,
      r.hits.length ? r.hits.map((c, i) => hitCard(c, i, marks, r.hits[0].score)) : empty(`Nothing in the index says “${S.q.trim()}”. Try fewer words, or a part of a name.`));
  }

  function startHere() {
    const recent = (S.index?.searches || []).filter((s) => s.kind === "code" && s.q).slice(0, 6);
    return h("section", { class: "lv-card lb-start" },
      h("h2", { class: "lv-h2" }, "Search as your agents do"),
      h("p", null, "Agents call ", h("code", { class: "lv-code" }, "search_code"), " instead of grepping: each answer is file:line ranges, or with a token budget one packed text. Their edits are reindexed as they make them, so each agent searches its own worktree."),
      recent.length ? h("div", { class: "lb-try" }, h("span", { class: "lv-sub" }, "What they searched lately:"),
        recent.map((s) => h("button", { class: "lb-chip", onclick: () => { S.q = s.q; search(); } }, avatar(s.agent, 14), s.q))) : null);
  }

  function hitCard(c, i, marks, top) {
    const lines = c.text.split("\n");
    const key = c.id || `${c.tree}:${c.path}:${c.start_line}`;
    const openAll = S.open.has(key);
    // A window of 10 lines around the first match, unless opened.
    let from = 0;
    if (!openAll && lines.length > 10 && marks) {
      const at = lines.findIndex((l) => { marks.lastIndex = 0; return marks.test(l); });
      from = Math.max(0, Math.min(lines.length - 10, at - 2));
    }
    const shown = openAll ? lines : lines.slice(from, from + 10);
    const edited = c.edited_by;
    const over = (c.overlaid_by || []).filter(Boolean);
    const card = h("article", { class: "lb-hit" + (S.focus === key ? " focus" : ""), tabindex: "0", "data-nav": "", "data-key": "hit-" + key,
      onkeydown: (e) => {
        if (e.target !== card) return;
        if (e.key === "o" || e.key === "Enter") { e.preventDefault(); toggle(); }
        if (e.key === "y") { e.preventDefault(); why(c.start_line + from); }
      } },
      h("header", { class: "lb-hit-h" },
        h("i", { class: "lb-dot", style: { background: langColor(c.lang) }, title: c.lang }),
        pathNode(c.path), h("span", { class: "lb-lines" }, `:${range(c)}`),
        c.symbol ? h("span", { class: "lb-sym" }, h("span", null, c.kind === "heading" ? "§" : c.kind), " ", c.symbol) : null,
        edited ? h("span", { class: "lb-edited", title: `This chunk is ${nameOf(edited)}'s version, from its worktree` }, icon("edit", "i sm"), avatar(edited, 14), `${nameOf(edited)}'s edit`) : null,
        over.length ? h("span", { class: "lb-over", title: `Also changed in ${over.map(nameOf).join(", ")}'s worktree` }, over.map((a) => avatar(a, 14)), `changed in ${over.length === 1 ? nameOf(over[0]) + "'s worktree" : over.length + " worktrees"}`) : null,
        h("span", { class: "lb-score", title: `BM25 score ${c.score}` }, h("i", null, h("b", { style: { width: Math.max(8, Math.round((100 * (c.score || 0)) / (top || 1))) + "%" } })), (c.score || 0).toFixed(1))),
      h("div", { class: "lb-code" }, shown.map((l, j) => h("div", { class: "lb-ln", onclick: (e) => { if (e.altKey) why(c.start_line + from + j); } },
        h("span", { class: "lb-no" }, String(c.start_line + (openAll ? 0 : from) + j)),
        h("span", { class: "lb-src" }, l ? tintLine(l, c.lang, marks) : " ")))),
      h("footer", { class: "lb-hit-f" },
        lines.length > 10 ? h("button", { class: "lv-link", onclick: toggle }, openAll ? "Show less" : `All ${lines.length} lines`) : null,
        h("button", { class: "lb-act", title: "Why is this line here? (y)", onclick: () => why(c.start_line + from) }, "Why?"),
        h("button", { class: "lb-act", title: "Copy path:lines", onclick: () => copy(`${c.path}:${range(c).replace("–", "-")}`, key) }, icon("copy", "i sm"), S.copied === key ? "Copied" : `${c.path.split("/").pop()}:${range(c)}`)));
    function toggle() { if (S.open.has(key)) S.open.delete(key); else S.open.add(key); S.focus = key; render(); }
    function why(line) { ctx.openWhy && ctx.openWhy({ agent: c.tree && c.tree !== "main" ? c.tree : S.view || null, path: c.path, line, text: lines[line - c.start_line] }); }
    return card;
  }

  function packCard(p) {
    const pct = Math.min(100, Math.round((100 * (p.used || 0)) / (p.budget || 1)));
    const counts = {};
    for (const r of p.rows || []) { const k = r.decision.startsWith("skip") ? "skipped" : r.decision.startsWith("linked") ? "linked" : r.decision; counts[k] = (counts[k] || 0) + 1; }
    const text = String(p.text || "");
    const lines = text.split("\n");
    return h("section", { class: "lv-card lb-pack" },
      h("div", { class: "lb-pack-h" },
        icon("pack"), h("div", null, h("b", null, "What an agent gets"), h("div", { class: "lv-sub" }, h("code", { class: "lv-code" }, `PACK ${count(p.budget)} TOKENS`), " on the same ranking: whole chunks while they fit, then an excerpt of the next")),
        h("div", { class: "lb-meter", title: `${p.used} of ${p.budget} tokens` }, h("div", { class: "lb-meter-bar" }, h("b", { style: { width: pct + "%" } })), h("span", null, `${fmtTokens(p.used)} / ${fmtTokens(p.budget)}`)),
        h("button", { class: "btn ghost lb-sm", onclick: () => copy(text, "pack") }, icon("copy", "i sm"), S.copied === "pack" ? "Copied" : "Copy")),
      h("ol", { class: "lb-pack-rows" }, (p.rows || []).map((r) => {
        const kind = r.decision.startsWith("skip") ? "skip" : r.decision;
        return h("li", { class: "k-" + kind },
          h("span", { class: "lb-dec" }, kind === "skip" ? r.decision.replace("skip: ", "skipped: ") : kind),
          h("span", { class: "lb-pr", title: r.path }, `${r.path.split("/").pop()}:${range(r)}`), r.symbol ? h("span", { class: "lb-ps" }, r.symbol) : null,
          h("span", { class: "lb-pt" }, r.tokens ? `${count(r.tokens)} tok` : ""));
      })),
      h("pre", { class: "lb-pack-text" + (S.packOpen ? " open" : "") }, (S.packOpen ? lines : lines.slice(0, 9)).join("\n")),
      lines.length > 9 ? h("button", { class: "lv-link", onclick: () => { S.packOpen = !S.packOpen; render(); } }, S.packOpen ? "Fold the text" : `The whole text (${lines.length} lines)`) : null);
  }

  function copy(text, key) {
    try { navigator.clipboard.writeText(text); } catch {}
    S.copied = key; render();
    setTimeout(() => { if (S.copied === key) { S.copied = null; if (alive) render(); } }, 1400);
  }

  function indexCard() {
    const d = S.index;
    const ix = S.code?.index || d?.index;
    if (!ix) return h("section", { class: "lv-card" }, h("h2", { class: "lv-h2" }, "Index"), S.indexErr ? h("div", { class: "err" }, S.indexErr) : h("div", { class: "lv-sub" }, "Reading…"));
    const total = (ix.langs || []).reduce((s, l) => s + l.chunks, 0) || 1;
    return h("section", { class: "lv-card lb-ix" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Index"),
        h("button", { class: "btn lb-sm", disabled: S.reindexing || ix.state === "indexing", title: "Read every file again (only the changed ones are rechunked)", onclick: reindex },
          S.reindexing ? h("span", { class: "spin" }) : icon("refresh", "i sm"), "Reindex")),
      ix.state === "none" ? h("div", { class: "lv-sub" }, "Not indexed yet: the first search reads the repository.") : [
        h("div", { class: "lb-ix-n" }, h("b", null, count(ix.chunks)), " chunks ", h("span", { class: "lv-sub" }, `in ${count(ix.files)} files`)),
        h("div", { class: "lb-stack" }, (ix.langs || []).map((l) => h("b", { style: { flex: `${l.chunks} 1 0`, background: langColor(l.lang) }, title: `${l.lang}: ${l.files} files, ${l.chunks} chunks` }))),
        h("div", { class: "lb-keys" }, (ix.langs || []).slice(0, 6).map((l) => h("span", null, h("i", { style: { background: langColor(l.lang) } }), l.lang, h("em", null, `${Math.round((100 * l.chunks) / total)}%`)))),
        h("div", { class: "lb-ix-meta" }, ix.indexed_ms ? `full pass ${fmtAgo(ix.indexed_ms)}` : "", ix.took_ms != null ? ` · ${(ix.took_ms / 1000).toFixed(1)} s` : "", ix.head ? [" · HEAD ", h("code", { class: "lv-code" }, String(ix.head).slice(0, 7))] : null),
        (ix.overlays || []).length ? h("div", { class: "lb-ov" }, h("div", { class: "lb-ov-h" }, "Worktrees overlaid"),
          ix.overlays.map((o) => h("button", { class: "lb-ov-row" + (S.view === o.agent.id ? " on" : ""), title: o.files.join("\n"), onclick: () => { S.view = S.view === o.agent.id ? "" : o.agent.id; if (S.q) search(); else render(); } },
            avatar(o.agent, 18), h("span", { class: "lb-ov-n" }, nameOf(o.agent)), h("span", { class: "lb-ov-f" }, o.files.slice(0, 2).map((f) => f.split("/").pop()).join(", ") + (o.files.length > 2 ? ` +${o.files.length - 2}` : "")),
            h("span", { class: "lb-ov-c" }, `${o.files.length} file${o.files.length === 1 ? "" : "s"}`)))) : null,
      ]);
  }

  function freshCard() {
    const recent = S.index?.recent || [];
    if (!recent.length) return null;
    return h("section", { class: "lv-card" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Kept fresh"), h("span", { class: "lv-sub lb-small" }, "reindexed on each Edit and Write")),
      h("ul", { class: "lb-feed" }, recent.slice(0, 6).map((r) => h("li", null,
        r.agent ? avatar(r.agent, 18) : h("span", { class: "lb-full" }, icon("refresh", "i sm")),
        h("div", { class: "lb-feed-t" },
          r.why === "full" ? h("span", null, h("b", null, "Full pass"), ` · ${r.path}`) : r.why === "removed" ? h("span", null, h("b", null, nameOf(r.agent)), " removed ", h("code", { class: "lv-code" }, r.path)) : h("span", null, h("b", null, nameOf(r.agent)), " edited ", h("code", { class: "lv-code" }, r.path.split("/").pop())),
          h("span", { class: "lv-sub" }, `${count(r.chunks)} chunk${r.chunks === 1 ? "" : "s"} · ${fmtAgo(r.ts_ms)}`))))));
  }

  function searchesCard() {
    const list = (S.index?.searches || []).slice(0, 7);
    return h("section", { class: "lv-card" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "What agents searched"), h("span", { class: "lv-sub lb-small" }, "search_code · search_docs")),
      list.length ? h("ul", { class: "lb-feed" }, list.map((s) => h("li", { class: s.hits ? "" : "miss" },
        avatar(s.agent, 18),
        h("div", { class: "lb-feed-t" },
          h("button", { class: "lb-qlink", title: s.kind === "docs" ? "Search the docs for it" : "Search the code for it", onclick: () => {
            if (s.kind === "docs") { S.dq = s.q; setTab("docs"); searchDocs(); } else { S.q = s.q; search(); }
          } }, icon(s.kind === "docs" ? "doc" : "code", "i sm"), s.q),
          h("span", { class: "lv-sub" }, s.hits ? `${s.hits} hit${s.hits === 1 ? "" : "s"}` : "nothing found", s.tokens ? ` · packed ${fmtTokens(s.tokens)}` : "", ` · ${nameOf(s.agent)} · ${fmtAgo(s.ts_ms)}`))))) :
        h("div", { class: "lv-sub" }, "No agent has searched yet. Agents started here get search_code and search_docs as MCP tools."));
  }

  // ------------------------------------------------------------------ docs
  function docsTab() {
    const d = S.lib;
    if (!d) return S.libErr ? h("div", { class: "err" }, S.libErr) : empty("Reading the repository's dependencies…");
    const libs = d.libraries || [];
    const sel = libs.find((l) => l.id === S.libSel);
    return h("div", { class: "lb-docs" },
      h("section", { class: "lv-card lb-libs" },
        h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Libraries ", h("span", { class: "n" }, libs.length)),
          libs.some((l) => l.state === "none") && d.crawler?.available !== false ? h("button", { class: "btn lb-sm", disabled: S.crawlBusy.has("all"), onclick: () => crawlAll(libs) }, S.crawlBusy.has("all") ? h("span", { class: "spin" }) : icon("globe", "i sm"), "Crawl the rest") : null),
        h("div", { class: "lb-mani" }, "from ", (d.manifests || []).map((m, i) => [i ? ", " : "", h("code", { class: "lv-code" }, m)])),
        libs.length ? h("ul", { class: "lb-lib-list" }, libs.map(libRow)) : empty("No Cargo.toml, package.json, pyproject.toml or go.mod with dependencies here."),
        h("form", { class: "lb-add", onsubmit: (e) => { e.preventDefault(); const u = S.addUrl.trim(); if (u) { crawl({ url: u }, "add"); S.addUrl = ""; } } },
          h("input", { type: "url", placeholder: "Another doc site: https://…", value: S.addUrl, "data-key": "add", "aria-label": "Add a doc site", oninput: (e) => { S.addUrl = e.target.value; } }),
          h("button", { class: "btn lb-sm", type: "submit", disabled: S.crawlBusy.has("add") }, "Add")),
        h("div", { class: "lb-crawler" }, crawlerNote(d.crawler)),
        S.libErr ? h("div", { class: "err" }, S.libErr) : null),
      h("div", { class: "lb-docs-main" }, docSearch(sel), sel ? pagesCard(sel) : null),
      h("aside", { class: "lb-side" }, citationsCard(d.citations || [])));
  }
  function crawlAll(libs) { crawl({ libraries: libs.filter((l) => l.state === "none").map((l) => l.id) }, "all"); }

  function crawlerNote(c) {
    if (!c) return null;
    if (c.available === false) return h("span", { class: "lb-warn" }, c.why || "No crawler is available.");
    return h("span", null, c.kind === "chloedb-crawl" ? "Fetched by chloedb-crawl" : `Fetched by Workbench${c.via ? ` (${c.via})` : ""}`, c.note ? `: ${c.note}` : "", ". Pages are rows of doc_pages, searched by BM25.");
  }

  function libRow(l) {
    const on = l.id === S.libSel;
    const state = l.state === "done" ? h("span", { class: "lb-st ok" }, `${fmtNum(l.pages)} pages · ${fmtAgo(l.crawled_ms)}`)
      : l.state === "crawling" || l.state === "queued" ? h("span", { class: "lb-st run" }, h("span", { class: "lb-mini" }, h("b", { style: { width: (l.progress?.total ? Math.round((100 * l.progress.done) / l.progress.total) : 4) + "%" } })), l.progress ? `${l.progress.done} / ${l.progress.total}` : "queued")
      : l.state === "failed" ? h("span", { class: "lb-st bad", title: l.error || "" }, "failed")
      : h("button", { class: "lb-crawl", disabled: S.crawlBusy.has(l.id), onclick: (e) => { e.stopPropagation(); crawl({ library: l.id }, l.id); } }, S.crawlBusy.has(l.id) ? "…" : "Crawl");
    return h("li", { class: "lb-lib" + (on ? " on" : "") + (l.state === "none" ? " cold" : ""), tabindex: "0", "data-nav": "", "data-key": "lib-" + l.id,
      onclick: () => { S.libSel = l.id; S.pages = null; save(); render(); loadPages(); if (S.dq && S.docScope === "lib") searchDocs(); },
      onkeydown: (e) => { if (e.key === "Enter" && e.target === e.currentTarget) e.currentTarget.click(); } },
      h("span", { class: "lb-eco e-" + l.ecosystem, title: ECO[l.ecosystem] || l.ecosystem }, (ECO[l.ecosystem] || l.ecosystem || "?").slice(0, 2)),
      h("div", { class: "lb-lib-t" }, h("div", null, h("b", null, l.name), l.version ? h("span", { class: "lb-ver" }, l.version) : null),
        h("div", { class: "lb-lib-s" }, l.state === "failed" ? h("span", { class: "lb-bad" }, l.error || "the crawl failed") : hostOf(l.url))),
      h("div", { class: "lb-lib-r" }, state,
        l.cited ? h("span", { class: "lb-cited", title: `cited ${l.cited} times by ${(l.cited_by || []).map(nameOf).join(", ")}` }, (l.cited_by || []).slice(0, 3).map((a) => avatar(a, 14)), `${l.cited}`) : null));
  }

  function docSearch(sel) {
    const input = h("input", { type: "search", value: S.dq, "data-key": "dq", placeholder: S.docScope === "lib" && sel ? `Search ${sel.name}'s docs…` : "Search every crawled doc page…", "aria-label": "Search the docs", autocomplete: "off",
      oninput: (e) => { S.dq = e.target.value; searchDocsSoon(); }, onkeydown: (e) => { if (e.key === "Enter") { searchDocsSoon.cancel(); searchDocs(); } } });
    const r = S.docs;
    const marks = marksOf(S.dq);
    return h("div", { class: "lb-dsearch" },
      h("div", { class: "lb-bar" },
        h("label", { class: "lb-q lb-dq" }, icon("search"), input),
        h("div", { class: "lb-scope", role: "radiogroup", "aria-label": "Where" },
          [["all", "All docs"], ["lib", sel ? sel.name : "This library"]].map(([k, label]) => h("button", { role: "radio", "aria-checked": String((S.docScope || "all") === k), class: (S.docScope || "all") === k ? "on" : "", onclick: () => { S.docScope = k; if (S.dq) searchDocs(); else render(); } }, label)))),
      S.docsErr ? h("div", { class: "err" }, S.docsErr) : null,
      r && S.dq.trim() ? h("section", { class: "lv-card lb-dhits" },
        h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, `${r.hits.length} page${r.hits.length === 1 ? "" : "s"} for “${S.dq.trim()}”`), h("span", { class: "lv-sub lb-small" }, "what search_docs would answer, with the URLs to cite")),
        r.hits.length ? h("ol", { class: "lb-dlist" }, r.hits.map((p) => h("li", null,
          h("a", { href: p.url, target: "_blank", rel: "noopener noreferrer", class: "lb-dt" }, markWords(p.title, marks)),
          h("div", { class: "lb-du" }, h("span", { class: "lb-dlib" }, p.library?.name || ""), " ", hostOf(p.url), h("span", { class: "lb-dir" }, pathOfUrl(p.url))),
          h("p", { class: "lb-dx" }, markWords(p.text || "", marks))))) : empty("No crawled page says that. Crawl more libraries, or try other words.")) : null);
  }

  function pagesCard(l) {
    const p = S.pages;
    const pages = p?.library?.id === l.id ? p.pages || [] : [];
    const head = h("div", { class: "lb-libhead" },
      h("div", null, h("div", { class: "lb-libname" }, h("b", null, l.name), l.version ? h("span", { class: "lb-ver" }, l.version) : null, h("span", { class: "lb-eco-t" }, ECO[l.ecosystem] || l.ecosystem), l.manifest ? h("code", { class: "lv-code" }, l.manifest) : null),
        h("a", { class: "lb-url", href: l.url, target: "_blank", rel: "noopener noreferrer" }, icon("link", "i sm"), l.url)),
      h("div", { class: "lb-libact" },
        l.state === "done" ? h("span", { class: "lv-sub" }, `${fmtNum(l.pages)} pages · crawled ${fmtAgo(l.crawled_ms)}`) : null,
        l.state === "crawling" || l.state === "queued" ? h("span", { class: "lb-st run" }, h("span", { class: "spin" }), l.progress ? `${l.progress.done} of ${l.progress.total} pages` : "queued") : null,
        l.state !== "crawling" && l.state !== "queued" ? h("button", { class: "btn lb-sm" + (l.state === "none" ? " primary" : ""), disabled: S.crawlBusy.has(l.id), onclick: () => crawl({ library: l.id }, l.id) },
          icon(l.state === "none" ? "globe" : "refresh", "i sm"), l.state === "none" ? "Crawl its docs" : "Crawl again") : null));
    let body;
    if (l.state === "failed") body = h("div", { class: "lb-fail" }, h("b", null, "The crawl failed. "), l.error || "");
    else if (l.state === "none") body = h("div", { class: "lb-cold" }, `Not crawled yet. Crawling reads ${hostOf(l.url)} from this page and the pages it links to on the same site (one level deep, at most 200 pages, robots.txt respected) into rows of doc_pages; agents then find them with search_docs.`);
    else if (!pages.length) body = S.pagesErr ? h("div", { class: "err" }, S.pagesErr) : h("div", { class: "lv-sub" }, l.state === "crawling" ? "The first pages arrive in a moment…" : "Reading…");
    else {
      const sorted = [...pages].sort((a, b) => (b.cited || 0) - (a.cited || 0) || (a.depth || 0) - (b.depth || 0) || String(a.title).localeCompare(String(b.title)));
      body = h("table", { class: "lb-pages" },
        h("thead", null, h("tr", null, h("th", null, "Page"), h("th", { class: "num" }, "Words"), h("th", null, "Cited by"))),
        h("tbody", null, sorted.slice(0, 60).map((pg) => h("tr", { class: pg.cited ? "cited" : "" },
          h("td", null, h("a", { href: pg.url, target: "_blank", rel: "noopener noreferrer" }, pg.title || pathOfUrl(pg.url)), h("div", { class: "lb-pu" }, pathOfUrl(pg.url))),
          h("td", { class: "num" }, fmtNum(pg.words)),
          h("td", null, (pg.cited_by || []).length ? h("div", { class: "lb-cb" }, (pg.cited_by || []).slice(0, 3).map((c) => h("span", { class: "lb-cbi", title: `${nameOf(c.agent)} · “${c.q}” · ${fmtAgo(c.ts_ms)}` }, avatar(c.agent, 16), h("span", null, `“${c.q}”`)))) : h("span", { class: "lv-sub" }, "—"))))));
    }
    return h("section", { class: "lv-card lb-pagescard" }, head, body);
  }

  function citationsCard(list) {
    return h("section", { class: "lv-card" },
      h("div", { class: "lv-card-head" }, h("h2", { class: "lv-h2" }, "Cited by agents"), h("span", { class: "lv-sub lb-small" }, "from search_docs")),
      list.length ? h("ul", { class: "lb-feed lb-cites" }, list.slice(0, 12).map((c) => h("li", null, avatar(c.agent, 18),
        h("div", { class: "lb-feed-t" },
          h("span", null, h("b", null, nameOf(c.agent)), " cited ", h("a", { href: c.url, target: "_blank", rel: "noopener noreferrer" }, c.title || pathOfUrl(c.url))),
          h("span", { class: "lv-sub" }, c.library ? `${c.library} · ` : "", "for ", h("button", { class: "lb-qin", onclick: () => { S.dq = c.q; searchDocs(); } }, `“${c.q}”`), ` · ${fmtAgo(c.ts_ms)}`))))) :
        h("div", { class: "lv-sub" }, "No agent has cited a page yet."));
  }

  // ---------------------------------------------------------------- keys
  const dropKeys = keyNav(el, { items: ".lb-hit, .lb-lib", onKey: (e) => {
    if (e.key === "/" && !e.shiftKey) { el.querySelector(S.tab === "code" ? ".lb-q input" : ".lb-dq input")?.focus(); return true; }
    if (e.key === "1" || e.key === "2") { setTab(e.key === "1" ? "code" : "docs"); return true; }
    return false;
  } });

  const refreshIndex = later(() => loadIndex(), 1500);
  let lastRepo = repo();

  // ----------------------------------------------------------------- start
  render();
  if (!repo()) { /* update() loads once a repository is known */ }
  else if (S.tab === "code") { loadIndex(); if (S.q) search(); } else { loadLibs(); loadIndex(); if (S.dq) searchDocs(); }
  requestAnimationFrame(() => el.querySelector(S.tab === "code" ? ".lb-q input" : ".lb-dq input")?.focus({ preventScroll: true }));

  return {
    update(state) {
      if (state) ctx.state = state;
      // The repository is known once an agent works in one.
      if (repo() !== lastRepo) {
        lastRepo = repo();
        Object.assign(S, { index: null, code: null, lib: null, pages: null, docs: null });
        render();
        if (lastRepo) { loadIndex(); if (S.q) search(); if (S.tab === "docs") loadLibs(); }
        return;
      }
      // A new edit event: the index's feed is a little stale; refetch it now and then.
      if (state && S.tab === "code" && (state.events || []).some((e) => e.kind === "PostToolUse" && toMs(e.ts) > (S.lastEv || 0))) {
        S.lastEv = Date.now();
        refreshIndex();
      }
    },
    unmount() { alive = false; clearTimeout(poll); searchSoon.cancel(); searchDocsSoon.cancel(); refreshIndex.cancel(); dropKeys(); el.remove(); },
  };
}

useStyle("library", `
.lb{padding:20px 24px 28px;display:flex;flex-direction:column;gap:14px}
.lb-head{align-items:center}
.lb-head .rp-switch{order:3}
.lb-tabs{display:inline-flex;margin-left:auto;background:var(--surface);border:1px solid var(--line);border-radius:999px;padding:3px;gap:2px}
.lb-tabs button{display:inline-flex;align-items:center;gap:7px;border:0;background:none;color:var(--text-2);font:inherit;font-weight:600;padding:5px 14px;border-radius:999px;cursor:pointer}
.lb-tabs button.on{background:var(--accent-soft);color:var(--accent)}
.lb-tab-n{font-weight:500;color:var(--muted);font-size:12px}.lb-tabs button.on .lb-tab-n{color:inherit;opacity:.8}
.lb-grid{display:grid;grid-template-columns:minmax(0,1fr) 360px;gap:18px;align-items:start}
.lb-main{display:flex;flex-direction:column;gap:10px;min-width:0}
.lb-side{display:flex;flex-direction:column;gap:14px;min-width:0}
.lb-bar{display:flex;gap:8px;align-items:center;flex-wrap:wrap}
.lb-q{flex:1 1 320px;display:flex;align-items:center;gap:9px;height:42px;padding:0 12px;background:var(--surface);border:1px solid var(--line-2);border-radius:10px;box-shadow:var(--shadow-1)}
.lb-q:focus-within{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.lb-q svg{color:var(--muted)}
.lb-q input:focus-visible{outline:0}
.lb-q input{flex:1;min-width:0;border:0;outline:0;background:none;font:15px var(--mono);color:var(--text)}
.lb-q input::placeholder{font-family:var(--sans);color:var(--muted)}
.lb-q input::-webkit-search-cancel-button{display:none}
.lb-sel{white-space:nowrap;display:inline-flex;align-items:center;gap:7px;height:42px;padding:0 4px 0 12px;border:1px solid var(--line);border-radius:10px;background:var(--surface);color:var(--muted);font-size:12px}
.lb-sel select,.lb-pack-ctl select{border:0;background:none;font:inherit;font-size:13px;font-weight:600;color:var(--text);padding:6px 4px;cursor:pointer;outline:0}
.lb-pack-ctl{display:inline-flex;align-items:center;height:42px;border:1px solid var(--line);border-radius:10px;background:var(--surface);overflow:hidden}
.lb-pack-btn{display:inline-flex;align-items:center;gap:6px;height:100%;border:0;background:none;color:var(--text-2);font:inherit;font-weight:600;padding:0 12px;cursor:pointer}
.lb-pack-btn.on{background:var(--accent-soft);color:var(--accent)}
.lb-langs{display:flex;gap:6px;flex-wrap:wrap}
.lb-lang{display:inline-flex;align-items:center;gap:6px;height:26px;padding:0 10px;border:1px solid var(--line);border-radius:999px;background:var(--surface);color:var(--text-2);font:inherit;font-size:12px;cursor:pointer}
.lb-lang i{width:8px;height:8px;border-radius:3px}.lb-lang span{color:var(--muted);font-size:11px}
.lb-lang.on{border-color:var(--accent);color:var(--text);box-shadow:0 0 0 2px var(--focus)}
.lb-meta{color:var(--muted);font-size:12.5px;display:flex;align-items:center;flex-wrap:wrap;gap:3px}
.lb-meta b{color:var(--text)}
.lb-missing{font-size:12px;color:var(--warn-ink);background:var(--warn-soft);border-radius:7px;padding:6px 10px}
.lb-results{display:flex;flex-direction:column;gap:10px}
.lb-hit{background:var(--surface);border:1px solid var(--line);border-radius:12px;box-shadow:var(--shadow-1);overflow:hidden;outline:0}
.lb-hit:focus,.lb-hit.focus{border-color:var(--accent);box-shadow:0 0 0 3px var(--focus)}
.lb-hit-h{display:flex;align-items:center;gap:8px;padding:9px 12px;border-bottom:1px solid var(--line);flex-wrap:wrap;min-width:0}
.lb-dot{width:9px;height:9px;border-radius:3px;flex:none}
.lb-path{font:12.5px var(--mono);color:var(--text);min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lb-dir{color:var(--muted)}
.lb-lines{font:12.5px var(--mono);color:var(--muted);margin-left:-6px}
.lb-sym{font:12px var(--mono);background:var(--surface-3);color:var(--text);border-radius:6px;padding:1px 7px}
.lb-sym span{color:var(--muted)}
.lb-edited,.lb-over{display:inline-flex;align-items:center;gap:5px;font-size:11.5px;font-weight:600;border-radius:999px;padding:2px 8px 2px 6px}
.lb-edited{background:var(--good-soft);color:var(--good-ink)}
.lb-over{background:var(--warn-soft);color:var(--warn-ink)}
.lb-edited .lv-av,.lb-over .lv-av{border-radius:4px;font-size:9px}
.lb-score{margin-left:auto;display:inline-flex;align-items:center;gap:7px;font:11.5px var(--mono);color:var(--muted)}
.lb-score i{display:block;width:54px;height:5px;border-radius:999px;background:var(--surface-3);overflow:hidden}.lb-score b{display:block;height:100%;background:var(--accent);border-radius:999px}
.lb-code{font:12.5px/1.6 var(--mono);padding:6px 0;background:var(--surface-2);overflow-x:auto}
.lb-ln{display:flex;min-width:max-content;padding-right:14px}
.lb-ln:hover{background:var(--surface-3)}
.lb-no{width:46px;flex:none;text-align:right;padding-right:14px;color:var(--muted);opacity:.7;user-select:none}
.lb-src{white-space:pre;color:var(--text)}
.lb-src mark{background:color-mix(in srgb,var(--warn) 30%,transparent);color:inherit;border-radius:3px;padding:0 1px}
.lb-t-kw{color:var(--lane-4);font-weight:600}.lb-t-str{color:var(--good-ink)}.lb-t-com{color:var(--muted);font-style:italic}.lb-t-num{color:var(--lane-2)}.lb-t-ty{color:var(--lane-1)}
.lb-hit-f{display:flex;align-items:center;gap:12px;padding:7px 12px;border-top:1px solid var(--line);font-size:12px}
.lb-act{display:inline-flex;align-items:center;gap:5px;border:0;background:none;color:var(--text-2);font:inherit;font-size:12px;cursor:pointer;padding:2px 6px;border-radius:6px}
.lb-act:hover{background:var(--surface-3);color:var(--text)}
.lb-hit-f .lb-act:first-of-type{margin-left:auto}
.lb-hit-f .lv-link{font-size:12px}
.lb-pack{display:flex;flex-direction:column;gap:10px;border-color:color-mix(in srgb,var(--accent) 40%,var(--line))}
.lb-pack-h{display:flex;align-items:center;gap:12px}
.lb-pack-h>svg{width:20px;height:20px;color:var(--accent)}
.lb-meter{margin-left:auto;display:flex;align-items:center;gap:8px;font:12px var(--mono);color:var(--text-2)}
.lb-meter-bar{width:140px;height:8px;border-radius:999px;background:var(--surface-3);overflow:hidden}.lb-meter-bar b{display:block;height:100%;background:var(--accent);border-radius:999px}
.lb-sm{height:28px;padding:0 10px;font-size:12.5px}
.lb-pack-rows{list-style:none;margin:0;padding:0;display:grid;grid-template-columns:repeat(auto-fill,minmax(300px,1fr));gap:4px 16px}
.lb-pack-rows li{display:flex;align-items:center;gap:8px;font-size:12px;min-width:0}
.lb-dec{font-size:10.5px;font-weight:700;text-transform:uppercase;letter-spacing:.04em;border-radius:5px;padding:1px 6px;flex:none}
.k-take .lb-dec{background:var(--good-soft);color:var(--good-ink)}.k-excerpt .lb-dec{background:var(--warn-soft);color:var(--warn-ink)}.k-skip .lb-dec{background:var(--surface-3);color:var(--muted);text-transform:none;letter-spacing:0}
.lb-pr{font-family:var(--mono);color:var(--text);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0}
.lb-ps{font-family:var(--mono);color:var(--muted);white-space:nowrap}.lb-pt{margin-left:auto;font-family:var(--mono);color:var(--muted);white-space:nowrap}
.lb-pack-text{margin:0;font:11.5px/1.55 var(--mono);background:var(--sunken);border:1px solid var(--line);border-radius:8px;padding:10px 12px;max-height:170px;overflow:hidden;white-space:pre;color:var(--text-2);position:relative;-webkit-mask-image:linear-gradient(#000 70%,transparent)}
.lb-pack-text.open{max-height:none;overflow:auto;-webkit-mask-image:none}
.lb-start{display:flex;flex-direction:column;gap:8px}.lb-start p{margin:0;color:var(--text-2);max-width:72ch}
.lb-try{display:flex;flex-wrap:wrap;gap:6px;align-items:center}
.lb-chip{display:inline-flex;align-items:center;gap:6px;border:1px solid var(--line);background:var(--surface-2);border-radius:999px;padding:3px 10px 3px 4px;font:12px var(--mono);color:var(--text);cursor:pointer}
.lb-chip:hover{border-color:var(--line-2)}
.lb-indexing{display:flex;gap:12px;align-items:center}
.lb-ix-n{font-size:13px;color:var(--text-2)}.lb-ix-n b{font-size:26px;font-weight:700;color:var(--text);letter-spacing:-.01em}
.lb-stack{display:flex;height:8px;border-radius:999px;overflow:hidden;gap:2px;margin:10px 0 8px}.lb-stack b{display:block;min-width:3px}
.lb-keys{display:flex;flex-wrap:wrap;gap:4px 12px;font-size:12px;color:var(--text-2)}
.lb-keys span{display:inline-flex;align-items:center;gap:5px}.lb-keys i{width:8px;height:8px;border-radius:3px}.lb-keys em{font-style:normal;color:var(--muted)}
.lb-ix-meta{margin-top:10px;font-size:12px;color:var(--muted)}
.lb-ov{margin-top:12px;border-top:1px solid var(--line);padding-top:10px;display:flex;flex-direction:column;gap:2px}
.lb-ov-h{font-size:11px;font-weight:650;color:var(--muted);text-transform:uppercase;letter-spacing:.06em;margin-bottom:4px}
.lb-ov-row{display:flex;align-items:center;gap:8px;border:0;background:none;font:inherit;color:var(--text);padding:5px 6px;border-radius:8px;cursor:pointer;text-align:left;min-width:0}
.lb-ov-row:hover{background:var(--surface-3)}.lb-ov-row.on{background:var(--accent-soft)}
.lb-ov-n{font-weight:600}.lb-ov-f{font:11.5px var(--mono);color:var(--muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0;flex:1}
.lb-ov-c{font-size:11.5px;color:var(--muted);white-space:nowrap}
.lb-small{font-size:11.5px}
.lb-feed{list-style:none;margin:0;padding:0;display:flex;flex-direction:column;gap:10px}
.lb-feed li{display:flex;gap:9px;align-items:flex-start;min-width:0}
.lb-feed li.miss .lb-qlink{color:var(--muted);text-decoration:line-through;text-decoration-color:var(--line-2)}
.lb-feed-t{display:flex;flex-direction:column;min-width:0;gap:1px;font-size:12.5px}
.lb-feed-t .lv-sub{font-size:11.5px}
.lb-feed-t a{color:var(--accent);text-decoration:none;font-weight:600}.lb-feed-t a:hover{text-decoration:underline}
.lb-full{display:inline-grid;place-items:center;width:18px;height:18px;border-radius:6px;background:var(--surface-3);color:var(--muted);flex:none}
.lb-qlink{display:inline-flex;align-items:center;gap:6px;border:0;background:none;padding:0;font:12.5px var(--mono);color:var(--text);cursor:pointer;text-align:left;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lb-qlink svg{color:var(--muted)}.lb-qlink:hover{color:var(--accent)}
.lb-qin{display:inline;border:0;background:none;padding:0;font:inherit;color:var(--text-2);cursor:pointer;text-align:left}.lb-qin:hover{color:var(--accent)}
.lb-docs{display:grid;grid-template-columns:380px minmax(0,1fr) 330px;gap:18px;align-items:start}
.lb-docs-main{display:flex;flex-direction:column;gap:12px;min-width:0}
.lb-libs{display:flex;flex-direction:column;gap:8px}
.lb-mani{font-size:12px;color:var(--muted);margin-top:-6px}
.lb-lib-list{list-style:none;margin:0 -8px;padding:0;display:flex;flex-direction:column;gap:2px}
.lb-lib{display:flex;align-items:center;gap:10px;padding:8px;border-radius:9px;cursor:pointer;outline:0;min-width:0}
.lb-lib:hover{background:var(--surface-2)}.lb-lib.on{background:var(--accent-soft)}
.lb-lib:focus-visible{box-shadow:0 0 0 2px var(--accent)}
.lb-lib.cold .lb-lib-t b{color:var(--text-2)}
.lb-eco{display:inline-grid;place-items:center;width:26px;height:26px;border-radius:7px;font:700 10px var(--mono);text-transform:lowercase;flex:none;color:#fff}
.lb-eco.e-crates{background:#c2521f}.lb-eco.e-npm{background:#c43b3b}.lb-eco.e-pypi{background:#2f6aa8}.lb-eco.e-go{background:#1a8aa3}.lb-eco.e-site{background:var(--muted)}
.lb-lib-t{display:flex;flex-direction:column;min-width:0;flex:1}
.lb-lib-t b{font-weight:650}
.lb-ver{font:11px var(--mono);color:var(--muted);margin-left:6px}
.lb-lib-s{font-size:11.5px;color:var(--muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lb-bad{color:var(--crit-ink)}
.lb-lib-r{display:flex;flex-direction:column;align-items:flex-end;gap:3px;flex:none}
.lb-st{font-size:11.5px;white-space:nowrap;display:inline-flex;align-items:center;gap:6px}
.lb-st.ok{color:var(--text-2)}.lb-st.run{color:var(--accent);font-family:var(--mono)}.lb-st.bad{color:var(--crit-ink);font-weight:600}
.lb-mini{display:inline-block;width:46px;height:5px;border-radius:999px;background:var(--surface-3);overflow:hidden}.lb-mini b{display:block;height:100%;background:var(--accent)}
.lb-crawl{border:1px solid var(--line-2);background:var(--surface);color:var(--text);font:inherit;font-size:12px;font-weight:600;border-radius:7px;padding:2px 10px;cursor:pointer}
.lb-crawl:hover{border-color:var(--accent);color:var(--accent)}
.lb-cited{display:inline-flex;align-items:center;gap:3px;font-size:11px;font-weight:700;color:var(--text-2)}
.lb-cited .lv-av{border-radius:4px;font-size:8.5px}
.lb-add{display:flex;gap:6px;margin-top:4px}
.lb-add input{flex:1;min-width:0;height:28px;border:1px solid var(--line-2);border-radius:7px;background:var(--surface-2);padding:0 9px;font-size:12.5px;outline:0}
.lb-add input:focus{border-color:var(--accent)}
.lb-crawler{font-size:11.5px;color:var(--muted);line-height:1.5}.lb-warn{color:var(--warn-ink)}
.lb-scope{display:inline-flex;border:1px solid var(--line);border-radius:10px;background:var(--surface);padding:3px;height:42px;align-items:center;gap:2px}
.lb-scope button{border:0;background:none;font:inherit;font-size:12.5px;font-weight:600;color:var(--text-2);padding:6px 11px;border-radius:7px;cursor:pointer;max-width:160px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lb-scope button.on{background:var(--accent-soft);color:var(--accent)}
.lb-dq input{font-family:var(--sans)}
.lb-dlist{margin:0;padding:0 0 0 18px;display:flex;flex-direction:column;gap:12px}
.lb-dlist li::marker{color:var(--muted);font-size:12px}
.lb-dt{font-weight:650;color:var(--accent);text-decoration:none}.lb-dt:hover{text-decoration:underline}
.lb-du{font:11.5px var(--mono);color:var(--muted);margin-top:1px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lb-dlib{font-family:var(--sans);font-weight:650;color:var(--text-2)}
.lb-dx{margin:4px 0 0;color:var(--text-2);font-size:12.5px;line-height:1.5}
.lb-dx mark,.lb-dt mark{background:color-mix(in srgb,var(--warn) 30%,transparent);color:inherit;border-radius:3px}
.lb-libhead{display:flex;justify-content:space-between;gap:12px;align-items:flex-start;margin-bottom:12px}
.lb-libname{display:flex;align-items:baseline;gap:8px;flex-wrap:wrap}.lb-libname b{font-size:17px}
.lb-eco-t{font-size:11.5px;color:var(--muted)}
.lb-url{display:inline-flex;align-items:center;gap:5px;font:12px var(--mono);color:var(--accent);text-decoration:none;margin-top:3px;word-break:break-all}
.lb-libact{display:flex;align-items:center;gap:10px;flex:none}
.lb-cold,.lb-fail{color:var(--text-2);background:var(--surface-2);border:1px dashed var(--line-2);border-radius:9px;padding:12px 14px;font-size:12.5px}
.lb-fail{border-style:solid;background:var(--crit-soft);color:var(--crit-ink);border-color:transparent}
.lb-pages{width:100%;border-collapse:collapse;font-size:12.5px;table-layout:fixed}
.lb-pages th:nth-child(3),.lb-pages td:nth-child(3){width:38%}
.lb-pages td{overflow:hidden}
.lb-pages th{text-align:left;font-size:11px;font-weight:650;color:var(--muted);text-transform:uppercase;letter-spacing:.05em;padding:0 8px 6px;border-bottom:1px solid var(--line)}
.lb-pages td{padding:7px 8px;border-bottom:1px solid var(--line);vertical-align:top}
.lb-pages tr:last-child td{border-bottom:0}
.lb-pages .num{text-align:right;font-family:var(--mono);color:var(--muted);width:70px}
.lb-pages a{color:var(--text);font-weight:600;text-decoration:none}.lb-pages a:hover{color:var(--accent);text-decoration:underline}
.lb-pages tr.cited a{color:var(--text)}
.lb-pu{font:11px var(--mono);color:var(--muted);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lb-pages td:first-child a{display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.lb-cb{display:flex;flex-direction:column;gap:4px}
.lb-cbi{display:flex;align-items:center;gap:6px;font-size:11.5px;color:var(--text-2);min-width:0}
.lb-cbi>span:last-child{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0}
.lb-cbi .lv-av{border-radius:4px;font-size:9px}
.lb-cites a{font-weight:600}
@media (max-width:1180px){.lb-grid,.lb-docs{grid-template-columns:minmax(0,1fr)}}
@media (max-width:640px){.lb{padding:14px}.lb-tabs{margin-left:0}}
`);
