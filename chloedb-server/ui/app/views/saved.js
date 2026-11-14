// Saved searches: a search kept as a stored function that returns rows
// (CREATE FUNCTION ... RETURNS TABLE, versioned in _functions), so that
// one definition is a query any SQL calls, an HTTP call, a line of Python
// or JavaScript, and a tool of the server's MCP endpoint (fn_<name>). The
// page lists them (GET /v1/functions), tries one, shows the code that
// calls it, and its versions, any of which it brings back.
//
// `useFromCode` and `saveSearch` serve the other pages: a table's search
// (views/table.js) and the query builder save from there.

import { h, icon, clear, api, card, empty, failure, fmt, toast, copy, modal, confirm, gate, can, sqlBlock, seg, store, at } from "../core.js";
import { isProduction } from "../data.js";
import { grid, answerStats } from "../results.js";

export default { id: "saved", title: "Saved searches", icon: "bookmark", group: "Build", range: false, keywords: "saved searches stored functions function endpoint api mcp tool reuse version rollback create function returns table", render, onParams };

let reshow = null;
function onParams(r) { reshow?.(r); }

const isVector = (ty) => /^vector/i.test(ty || "");
/** The tables a definition reads (FROM and JOIN names). */
const readsOf = (sql) => [...new Set([...(sql || "").matchAll(/\b(?:FROM|JOIN)\s+"?([A-Za-z_][\w]*)"?(?!\s*\()/gi)].map((m) => m[1]).filter((t) => !/^(select|unnest)$/i.test(t)))];
/** Its calls over the last 15 minutes, from the statements that name it: {calls, errors, p99, keys}. */
async function usage() {
  // The statements' statistics are an admin's: another key asks nothing.
  if (!can("admin")) return null;
  try {
    const st = await api("/v1/queries/stats?limit=500");
    const list = Array.isArray(st) ? st : st.statements || [];
    return (name) => {
      const re = new RegExp(`\\b${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*\\(`, "i");
      const sts = list.filter((x) => re.test(x.sql || x.example || x.fingerprint || ""));
      const keys = new Map();
      for (const x of sts) for (const k of x.keys || []) if (k.key) keys.set(k.key, (keys.get(k.key) || 0) + (k.calls || 0));
      return { calls: sts.reduce((a, x) => a + (x.calls || 0), 0), errors: sts.reduce((a, x) => a + (x.errors || 0), 0), p99: sts.length ? Math.max(...sts.map((x) => x.p99_ms ?? 0)) : null, keys, sts };
    };
  } catch { return null; }
}
/** A text with its runs of white space made one space, for comparing definitions. */
const squash = (s) => String(s || "").replace(/\s+/g, " ").trim();
/**
 * Whether a function's version `v` (as GET /v1/functions lists it) is the
 * definition a save would write: its name, parameters, body and
 * description, white space aside. The server makes no version for it.
 */
export function sameDefinition(v, { name, params, body, description }) {
  const sig = `${name}(${params.map((p) => `${p.name} ${p.ty}${p.default != null ? ` DEFAULT ${p.default}` : ""}`).join(", ")})`;
  return !!v && v.body != null && squash(v.body) === squash(body) && (v.description || "") === String(description || "").trim() && squash(v.signature) === squash(sig);
}
/** A line diff of two texts: [kind, line] with kind " ", "-" or "+". */
export function diff(a, b) {
  const x = (a || "").split("\n"), y = (b || "").split("\n");
  const n = x.length, m = y.length, L = Array.from({ length: n + 1 }, () => new Int32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) L[i][j] = x[i] === y[j] ? L[i + 1][j + 1] + 1 : Math.max(L[i + 1][j], L[i][j + 1]);
  const out = [];
  let i = 0, j = 0;
  while (i < n && j < m) { if (x[i] === y[j]) { out.push([" ", x[i]]); i++; j++; } else if (L[i + 1][j] >= L[i][j + 1]) out.push(["-", x[i++]]); else out.push(["+", y[j++]]); }
  while (i < n) out.push(["-", x[i++]]);
  while (j < m) out.push(["+", y[j++]]);
  return out;
}
/**
 * Two versions' difference, a clause a line: the server keeps each
 * definition on one line, so both are laid out by `format` (the
 * workbench's formatSql) before their lines are compared (CCB-18).
 */
export const versionDiff = (a, b, format = (s) => s) => diff(format(a || "").trimEnd(), format(b || "").trimEnd());
const diffView = (lines) => h("pre", { class: "code diff" }, lines.map(([k, l]) => h("div", { class: k === "+" ? "add" : k === "-" ? "del" : "" }, `${k} ${l}`)));
const origin = () => location.origin;
const js = (v) => JSON.stringify(v);
/** A parameter's sample value, for code. */
function sample(p) {
  const t = String(p.ty || p.type || "").toUpperCase();
  if (t.startsWith("INT")) return 10;
  if (t.startsWith("FLOAT")) return 0.5;
  if (t.startsWith("BOOL")) return true;
  if (t.startsWith("VECTOR")) return { embed: "words to search for" };
  return "words to search for";
}

/**
 * The code that runs `sql` with `params` (values by name): curl, Python,
 * JavaScript, and when it calls a saved search, its MCP tool. Returns a
 * card body with tabs.
 */
export function useFromCode({ sql, params = {}, tool = null }) {
  const body = js({ sql, params });
  const pyParams = Object.keys(params).length ? `, ${JSON.stringify(params, null, 0).replace(/\btrue\b/g, "True").replace(/\bfalse\b/g, "False").replace(/\bnull\b/g, "None")}` : "";
  const jsParams = Object.keys(params).length ? `, ${JSON.stringify(params)}` : "";
  const tabs = [
    ["curl", "curl", `curl -s ${origin()}/v1/query \\\n  -H "authorization: Bearer $CHLOEDB_TOKEN" \\\n  -H 'content-type: application/json' \\\n  -d '${body.replace(/'/g, "'\\''")}'`],
    ["python", "Python", `import chloedb  # pip install chloedb\n\ndb = chloedb.connect("${origin()}")  # the key from $CHLOEDB_TOKEN\nrows = db.query(${js(sql)}${pyParams}).rows`],
    ["js", "JavaScript", `import { connect } from "chloedb/client";  // npm install chloedb\n\nconst db = connect("${origin()}", { token: process.env.CHLOEDB_TOKEN });\nconst { rows } = await db.query(${js(sql)}${jsParams});`],
  ];
  if (tool) tabs.push(["mcp", "Agent (MCP)", `# Claude Code: the server's MCP endpoint, with a read key\nclaude mcp add --transport http chloedb ${origin()}/v1/mcp \\\n  --header "Authorization: Bearer $CHLOEDB_TOKEN"\n\n# The agent then has the tool ${tool}, with these arguments:\n${JSON.stringify(Object.fromEntries(Object.entries(params).map(([k, v]) => [k, v && v.embed != null ? v.embed : v])), null, 2)}`]);
  let pick = store.get("code.tab", "curl");
  if (!tabs.some((t) => t[0] === pick)) pick = "curl";
  const box = h("div");
  const draw = () => { const t = tabs.find((x) => x[0] === pick); clear(box, h("div", { class: "code-wrap" }, h("pre", { class: "code" }, t[2]), h("button", { class: "btn xs code-copy", title: "Copy", onclick: () => copy(t[2], "Code copied") }, icon("copy"), "Copy"))); };
  const tabsEl = seg(tabs.map(([k, label]) => [k, label]), pick, (v) => { pick = v; store.set("code.tab", v); draw(); });
  draw();
  return h("div", { class: "stack", style: { gap: "8px" } }, tabsEl, box,
    h("div", { class: "small muted" }, "A read key is enough to run it: create one in ", h("a", { href: "#/access?tab=keys&new=1" }, "Access and audit"), ", limited to the tables it reads."));
}

/** A function name from words: lower case letters, digits and _. */
export const slug = (s) => String(s || "").toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "").replace(/^(\d)/, "s_$1").slice(0, 48) || "my_search";

/**
 * Saves a search as a stored function: asks its name and what it is for,
 * then runs CREATE [OR REPLACE] FUNCTION name(params) RETURNS TABLE AS
 * body. `params`: [{name, ty, default?}]; `body` the SELECT, which names
 * them as :name. Resolves the name saved, or null.
 */
export function saveSearch({ suggested = "", description = "", params, body, table }) {
  return new Promise((resolve) => {
    const nameIn = h("input", { class: "input", value: slug(suggested), spellcheck: "false", style: { width: "100%" } });
    const aboutIn = h("input", { class: "input", value: description, placeholder: "What it finds, for people and agents", style: { width: "100%" } });
    const pre = h("div");
    // The functions there now, by name: each one's current version.
    let existing = null;
    const current = (fs) => new Map((fs || []).filter((f) => f.current != null).map((f) => [f.name, f.versions.find((v) => v.version === f.current) || {}]));
    api("/v1/functions").then((j) => { existing = current(j.functions); sync(); }).catch(() => {});
    const sig = params.map((p) => `${p.name} ${p.ty}${p.default != null ? ` DEFAULT ${p.default}` : ""}`).join(", ");
    const sqlOf = () => `CREATE ${existing?.has(slug(nameIn.value)) ? "OR REPLACE " : ""}FUNCTION ${slug(nameIn.value)}(${sig}) RETURNS TABLE OPTIONS(description = '${aboutIn.value.trim().replace(/'/g, "''")}') AS\n${body}`;
    const ok = h("button", { class: "btn primary" }, "Save");
    // A save of the definition there now writes nothing (CCB-17): the
    // server keeps the version it has, so the dialog says so.
    const sameAs = (n) => sameDefinition(existing?.get(n), { name: n, params, body, description: aboutIn.value });
    function sync() {
      const n = slug(nameIn.value), same = sameAs(n);
      clear(pre, same ? h("div", { class: "small muted" }, icon("okc"), ` ${n} is this search already (version ${existing.get(n).version}): saving it again makes no new version.`)
        : existing?.has(n) ? h("div", { class: "small", style: { color: "var(--serious)" } }, icon("warn"), ` ${n} exists: saving makes its next version (the old ones stay, and can come back).`) : null,
        h("details", {}, h("summary", { class: "small muted" }, "The statement"), sqlBlock(sqlOf())));
      ok.textContent = same ? "Keep it" : existing?.has(n) ? "Save a new version" : "Save";
    }
    nameIn.oninput = sync; aboutIn.oninput = sync;
    const m = modal({
      title: "Save this search",
      body: [h("p", { class: "dim", style: { margin: 0 } }, `It becomes a stored function${table ? ` over ${table}` : ""}: callable from SQL, over HTTP, from Python and JavaScript, and by agents as the MCP tool fn_<name>. Every save is a version you can go back to.`),
        h("label", { class: "f" }, "Name", nameIn), h("label", { class: "f" }, "What it finds", aboutIn), pre],
      foot: [h("button", { class: "btn", onclick: () => { m.close(); resolve(null); } }, "Cancel"), gate(ok, "admin")],
    });
    ok.onclick = async () => {
      const n = slug(nameIn.value);
      if (!aboutIn.value.trim()) { toast("Say what it finds: agents choose a tool by it", "bad"); aboutIn.focus(); return; }
      ok.disabled = true;
      const was = existing?.get(n)?.version;
      try {
        await api("/v1/sql", { body: { sql: sqlOf() } });
        m.close();
        // Whether a version was made is the server's to say: the same
        // definition again leaves the current one.
        const now = await api("/v1/functions").then((j) => current(j.functions).get(n)?.version).catch(() => undefined);
        if (was != null && now === was) toast(`${n} is unchanged: it stays at version ${was}`, "info");
        else toast(now != null && now > 1 ? `Saved ${n} as version ${now}` : `Saved as ${n}`, "ok");
        resolve(n);
      } catch (e) { ok.disabled = false; toast(e.message, "bad"); }
    };
    sync();
  });
}

// ------------------------------------------------------------ the page
function render(page, r) {
  const head = h("div", { class: "page-head" },
    h("div", {}, h("h1", {}, "Saved searches"), h("p", {}, "Searches kept as stored functions: one definition you call from SQL, over HTTP, from code, or as an agent's tool. Each save is a version.")),
    h("div", { class: "tools" }, h("a", { class: "btn", href: "#/table" }, icon("search"), "Search a table")));
  const body = h("div", { class: "stack" });
  page.append(head, body);
  let list = null, alive = true;

  let use = null;
  async function load() {
    try { [list, use] = await Promise.all([api("/v1/functions").then((j) => j.functions || []), usage()]); } catch (e) { clear(body, failure(e, "The saved searches")); return; }
    if (alive) show(r);
  }
  function show(route) {
    r = route;
    const name = r.rest?.[0];
    if (name) return detail(name);
    const live = list.filter((f) => f.current != null);
    const cur = (f) => f.versions.find((v) => v.version === f.current) || {};
    const tablesFns = live.filter((f) => cur(f).kind === "table"), scalars = live.filter((f) => cur(f).kind !== "table");
    if (!tablesFns.length && !scalars.length) {
      clear(body, card({ body: empty("bookmark", "No saved searches yet", "Search one of your tables, then Save: the search becomes a function, an endpoint and an agent's tool.", h("a", { class: "btn primary", href: "#/table" }, icon("search"), "Search a table")) }));
      return;
    }
    const row = (f) => {
      const v = cur(f);
      const u = use?.(f.name), reads = readsOf(v.sql || v.body);
      return h("a", { class: "saved-row", href: `#/saved/${encodeURIComponent(f.name)}` },
        h("div", { class: "grow", style: { minWidth: 0 } }, h("div", {}, h("b", { class: "mono" }, f.name), h("span", { class: "small muted" }, ` · version ${f.current}${f.versions.length > 1 ? ` of ${f.versions.length}` : ""}${reads.length ? ` · reads ${reads.join(", ")}` : ""}`)),
          h("div", { class: "small dim" }, v.description || h("i", {}, "No description: agents see only its signature")),
          h("div", { class: "small muted mono ellipsis" }, v.signature)),
        u ? h("div", { class: "saved-use small", title: "over the last 15 minutes" }, h("div", {}, h("b", {}, fmt.n(u.calls)), " calls"), h("div", { class: u.errors ? "bad" : "muted" }, `${fmt.n(u.errors)} failed`), h("div", { class: "muted" }, u.p99 != null ? `p99 ${fmt.ms(u.p99)}` : "")) : null,
        h("div", { class: "small muted", style: { textAlign: "right", minWidth: "90px" } }, v.created ? fmt.ago(Date.parse(v.created)) : "", v.created_by ? h("div", {}, "by " + v.created_by) : null),
        icon("right"));
    };
    clear(body,
      tablesFns.length ? card({ title: "Searches", sub: `${tablesFns.length} · each one is also the MCP tool fn_<name>${use ? " · calls over the last 15 minutes" : ""}`, flush: true, body: h("div", { class: "saved-list" }, tablesFns.map(row)) }) : null,
      scalars.length ? card({ title: "Other stored functions", sub: "functions that return a value, used inside queries (a boost, a score)", flush: true, body: h("div", { class: "saved-list" }, scalars.map(row)) }) : null);
  }
  reshow = (route) => { if (list) show(route); };

  function detail(name) {
    const f = list.find((x) => x.name === name);
    if (!f) { clear(body, card({ body: empty("bookmark", `No saved search ${name}`, "It may have been dropped.", h("a", { class: "btn", href: "#/saved" }, "All saved searches")) })); return; }
    const v = f.versions.find((x) => x.version === f.current) || f.versions[f.versions.length - 1];
    const params = parseSig(v.signature);
    const text = params.find((p) => /^text$/i.test(p.ty));
    const inputs = {};
    const form = h("div", { class: "saved-form" }, params.map((p) => {
      const vec = isVector(p.ty);
      const inp = h("input", { class: "input", placeholder: vec ? (text ? `the words of ${text.name}, embedded` : "words to embed") : p.default != null ? `default ${p.default}` : p.ty, value: store.get(`saved.arg.${name}.${p.name}`, "") || "" });
      inputs[p.name] = inp;
      inp.onkeydown = (e) => { if (e.key === "Enter") run(); };
      return h("label", { class: "f" }, h("span", {}, h("b", { class: "mono" }, p.name), h("span", { class: "small muted" }, ` ${p.ty}${p.default != null ? ` · default ${p.default}` : ""}`)), inp);
    }));
    const out = h("div");
    const codeBox = h("div");
    function args() {
      const a = {};
      for (const p of params) {
        const raw = inputs[p.name].value.trim();
        store.set(`saved.arg.${name}.${p.name}`, raw);
        if (isVector(p.ty)) { const w = raw || (text ? inputs[text.name].value.trim() : ""); if (w) a[p.name] = /^\[/.test(w) ? JSON.parse(w) : { embed: w }; continue; }
        if (!raw) continue;
        a[p.name] = /^(INT|FLOAT)/i.test(p.ty) ? Number(raw) : /^BOOL/i.test(p.ty) ? /^(true|1|yes)$/i.test(raw) : raw;
      }
      return a;
    }
    const callSql = (a) => `SELECT * FROM ${name}(${Object.keys(a).map((k) => `${k} => :${k}`).join(", ")})`;
    function drawCode() {
      let a = {}; try { a = args(); } catch { /* a vector being typed */ }
      const shown = Object.keys(a).length ? a : Object.fromEntries(params.filter((p) => p.default == null).map((p) => [p.name, sample(p)]));
      clear(codeBox, useFromCode({ sql: callSql(shown), params: shown, tool: v.kind === "table" ? `fn_${name}` : null }));
    }
    for (const i of Object.values(inputs)) i.addEventListener("input", drawCode);
    async function run() {
      let a;
      try { a = args(); } catch (e) { toast("A vector is a JSON array of numbers: " + e.message, "bad"); return; }
      const missing = params.find((p) => p.default == null && a[p.name] == null);
      if (missing) { toast(`${missing.name} is needed`, "bad"); inputs[missing.name].focus(); return; }
      out.classList.add("loading");
      try {
        const j = await api("/v1/query", { body: { sql: callSql(a), params: a } });
        clear(out, h("div", { class: "bd", style: { paddingBottom: "4px" } }, answerStats(j)), j.rows?.length ? h("div", { class: "tbl-wrap" }, grid(j, { max: 200 })) : empty("search", "No rows", "Other words, or a looser search."));
      } catch (e) { clear(out, h("div", { class: "error" }, e.message)); }
      finally { out.classList.remove("loading"); }
      drawCode();
    }
    const versions = h("div", { class: "saved-versions" }, [...f.versions].reverse().map((x) => h("details", { open: x.version === f.current },
      h("summary", {}, h("b", {}, `Version ${x.version}`), x.version === f.current ? h("span", { class: "badge accent" }, "current") : null, x.dropped ? h("span", { class: "badge" }, "dropped") : null,
        h("span", { class: "small muted" }, ` ${x.created ? fmt.ago(Date.parse(x.created)) : ""}${x.created_by ? " by " + x.created_by : ""}`), h("span", { class: "grow" }),
        x.version !== f.current && !x.dropped && v.sql && x.sql ? h("button", { class: "btn xs", title: "The lines that differ from the current version", onclick: async (e) => { e.preventDefault(); const d = e.currentTarget.closest("details"); d.open = true; const box = d.querySelector(".ver-body"); box.dataset.diff = box.dataset.diff === "1" ? "" : "1"; const { formatSql } = box.dataset.diff ? await import("./sql.js") : {}; clear(box, box.dataset.diff ? [h("div", { class: "small muted" }, `version ${x.version} (−) against the current, ${f.current} (+)`), diffView(versionDiff(x.sql, v.sql, formatSql))] : sqlBlock(x.sql)); } }, icon("layers"), "Compare") : null,
        x.version !== f.current && !x.dropped ? gate(h("button", { class: "btn xs", title: "Make this version the current one again: saved as a new version with its definition", onclick: (e) => { e.preventDefault(); back(x.version); } }, icon("refresh"), "Bring back"), "admin") : null),
      h("div", { class: "ver-body" }, x.sql ? sqlBlock(x.sql) : h("div", { class: "small muted" }, "A drop: the function was removed here.")))));
    function usageCard() {
      const u = use?.(name);
      if (!u) return null;
      const reads = readsOf(v.sql || v.body);
      const tile = (label, val, sub, cls = "") => h("div", { class: "dt-tile " + cls }, h("div", { class: "small muted" }, label), h("div", { class: "big" }, val), sub ? h("div", { class: "small muted" }, sub) : null);
      return card({ title: "Its use", sub: "the last 15 minutes, from the statements that call it", body: h("div", { class: "stack", style: { gap: "10px" } },
        h("div", { class: "saved-tiles" }, tile("Calls", fmt.n(u.calls)), tile("Failed", fmt.n(u.errors), u.calls ? fmt.pct(u.errors / u.calls) : null, u.errors ? "bad" : ""), tile("p99", u.p99 != null ? fmt.ms(u.p99) : "–"), tile("Reads", reads.length ? reads.join(", ") : "–", reads.length === 1 ? h("a", { href: `#/table/${encodeURIComponent(reads[0])}` }, "its table") : null)),
        u.keys.size ? h("div", { class: "row small", style: { gap: "6px", flexWrap: "wrap" } }, "Called by: ", [...u.keys].sort((a, b) => b[1] - a[1]).map(([k, n]) => h("a", { class: "chip", href: `#/queries?key=${encodeURIComponent(k)}` }, icon("key"), `${k} · ${fmt.plural(n, "call")}`))) : h("div", { class: "small muted" }, u.calls ? "" : "Not called lately. Agents call it as the MCP tool ", h("span", { class: "mono" }, `fn_${name}`), u.calls ? null : "; code calls it as below."),
        u.sts.length ? h("div", { class: "stack", style: { gap: "0" } }, u.sts.slice(0, 5).map((x) => h("a", { class: "used-st", href: `#/statement/${encodeURIComponent(x.fingerprint || x.sql)}` }, h("span", { class: "mono small ellipsis grow" }, x.sql || x.example || x.fingerprint), h("span", { class: "small muted" }, `${fmt.plural(x.calls || 0, "call")}${x.p99_ms != null ? " · p99 " + fmt.ms(x.p99_ms) : ""}`)))) : null) });
    }
    async function back(n) {
      const ok = await confirm({ title: `Bring back version ${n} of ${name}?`, body: `Calls use version ${n}'s definition from now on, saved as version ${Math.max(...f.versions.map((x) => x.version)) + 1}. The current one stays, so this can be undone the same way.`, ok: "Bring it back", reason: isProduction() ? "Why: kept in the audit log" : false });
      if (!ok) return;
      try { await api("/v1/sql", { body: { sql: `CREATE OR REPLACE FUNCTION ${name} FROM VERSION ${n}` }, reason: ok.reason || undefined }); toast(`Version ${n} is current again`, "ok"); load(); } catch (e) { toast(e.message, "bad"); }
    }
    async function drop() {
      const ok = await confirm({ title: `Remove ${name}?`, body: "Queries and agents that call it fail from now on. Its versions stay in _functions, so it can be brought back.", ok: "Remove", danger: true, typed: name, reason: isProduction() ? "Why: kept in the audit log" : false });
      if (!ok) return;
      try { await api("/v1/sql", { body: { sql: `DROP FUNCTION ${name}` }, reason: ok.reason || undefined }); toast(`${name} removed`, "ok"); location.hash = "#/saved"; load(); } catch (e) { toast(e.message, "bad"); }
    }
    clear(body,
      h("div", { class: "row" }, h("a", { class: "btn ghost sm", href: "#/saved" }, icon("left"), "All saved searches")),
      card({ title: h("span", { class: "mono" }, name), sub: v.description || "no description", tools: [h("button", { class: "btn sm", onclick: () => copy(callSql(Object.fromEntries(params.map((p) => [p.name, 1]))), "SQL copied") }, icon("copy"), "Copy the call"), gate(h("button", { class: "btn sm danger", onclick: drop }, icon("trash"), "Remove"), "admin")],
        body: h("div", { class: "stack" }, h("div", { class: "small muted mono" }, v.signature), form, h("div", { class: "row" }, h("button", { class: "btn primary", onclick: run }, icon("play"), "Try it")), out) }),
      usageCard(),
      card({ title: "Use it", sub: "the same search from anywhere", body: codeBox }),
      card({ title: "Versions", sub: `${f.versions.length} kept · a version never changes`, tools: v.sql ? [gate(h("a", { class: "btn sm", title: "Open its definition in the SQL workbench: running it saves the next version", href: `#/sql?q=${encodeURIComponent(v.sql.replace(/^\s*CREATE\s+FUNCTION/i, "CREATE OR REPLACE FUNCTION"))}` }, icon("edit"), "Edit as a new version"), "admin")] : null, body: versions }));
    drawCode();
    if (params.every((p) => p.default != null || inputs[p.name].value || isVector(p.ty))) run();
  }

  load();
  return () => { alive = false; reshow = null; };
}

/** The parameters of a signature, "name(q TEXT, k INT DEFAULT 10)". */
export function parseSig(sig) {
  const inner = String(sig || "").replace(/^[^(]*\(/, "").replace(/\)\s*$/, "");
  if (!inner.trim()) return [];
  const out = [];
  let depth = 0, cur = "";
  for (const ch of inner) { if (ch === "(") depth++; if (ch === ")") depth--; if (ch === "," && !depth) { out.push(cur); cur = ""; } else cur += ch; }
  out.push(cur);
  return out.map((x) => { const m = x.trim().match(/^(\S+)\s+(.+?)(?:\s+DEFAULT\s+(.+))?$/i); return m ? { name: m[1], ty: m[2], default: m[3] ?? null } : null; }).filter(Boolean);
}
