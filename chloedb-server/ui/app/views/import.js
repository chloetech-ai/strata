// Import and export: drop a file (CSV, TSV, JSON Lines, JSON, a SQLite
// database or a Parquet file), see its first rows or the plan the server
// makes of it and the type each column reads as, load it into a table that
// exists or one made for it, and watch it upload. The database goes out as
// a SQLite file or a Parquet file a table, and the stream sources show how
// far each event stream has been written, with a box to send one.

import { h, icon, clear, fmt, api, at, authHeader, toast, empty, seg, sqlBlock, dropzone, every, failure, confirm, gate, can, bus } from "../core.js";
import { followsWho, writeThere, followBanner } from "../follows.js";
import { isProduction, isCoordinator } from "../data.js";
import { tables, cached, invalidate, table as tableOf, typeBadge, typeMeta, TYPES } from "../catalog.js";
import { quoteName } from "../sqledit.js";
import { grid } from "../results.js";
import { reach, whereName } from "../sqlguard.js";
/** "prod-eu1 (production)", or the host when the cluster has no name. */
const whereTo = () => { const { name, env } = whereName(); return env ? `${name} (${env})` : name; };

export default { id: "import", title: "Import and export", icon: "import", group: "Data", range: false, keywords: "import upload load csv tsv json jsonl ndjson sqlite parquet arrow spark file export download stream kafka sources offsets", render };

const SQLITE = "SQLite format 3\u0000";
/** A reason as a header value: one line, ASCII, non-ASCII percent-encoded. */
const safeHeader = (r) => String(r).replace(/[\r\n]+/g, " ").replace(/[^\x20-\x7e]/g, (c) => encodeURIComponent(c)).slice(0, 2000);
const CT = { csv: "text/csv", tsv: "text/tab-separated-values", jsonl: "application/x-ndjson", json: "application/json", sqlite: "application/vnd.sqlite3", parquet: "application/vnd.apache.parquet" };

/** The format of a file, from its first bytes and its name. */
export async function sniff(file) {
  const head = await file.slice(0, 4096).text();
  if (head.startsWith(SQLITE)) return "sqlite";
  if (head.startsWith("PAR1")) return "parquet";
  const ext = (file.name.split(".").pop() || "").toLowerCase();
  if (ext === "parquet") return "parquet";
  // A .json file of one object a line (as many tools write) is JSON Lines.
  if (ext === "json" && /^\s*\{/.test(head)) {
    const lines = head.trim().split("\n");
    try { if (lines.length > 1 && typeof JSON.parse(lines[0]) === "object" && /^\s*\{/.test(lines[1])) return "jsonl"; } catch { /* one object over many lines: JSON */ }
  }
  if (ext === "csv" || ext === "tsv" || ext === "json") return ext;
  if (ext === "jsonl" || ext === "ndjson") return "jsonl";
  if (ext === "sqlite" || ext === "db" || ext === "sqlite3") return "sqlite";
  const t = head.trimStart();
  if (t.startsWith("[")) return "json";
  if (t.startsWith("{")) return "jsonl";
  const first = t.split("\n")[0];
  return (first.match(/\t/g) || []).length > (first.match(/,/g) || []).length ? "tsv" : "csv";
}

/** CSV or TSV text as rows of strings, with quoted fields. */
export function parseDelimited(text, sep = ",", max = Infinity) {
  const rows = [];
  let row = [], cell = "", q = false, i = 0;
  for (; i < text.length && rows.length < max; i++) {
    const c = text[i];
    if (q) { if (c === '"') { if (text[i + 1] === '"') { cell += '"'; i++; } else q = false; } else cell += c; }
    else if (c === '"' && cell === "") q = true;
    else if (c === sep) { row.push(cell); cell = ""; }
    else if (c === "\n" || c === "\r") { if (c === "\r" && text[i + 1] === "\n") i++; row.push(cell); rows.push(row); row = []; cell = ""; }
    else cell += c;
  }
  if ((cell !== "" || row.length) && rows.length < max) { row.push(cell); rows.push(row); }
  return rows;
}

/**
 * The separator of a CSV whose header holds more ';' (or '|') than ',',
 * as spreadsheets in much of Europe write it; null for commas.
 */
export function otherSeparator(firstLine) {
  const n = (c) => firstLine.split(c).length - 1;
  const comma = n(",");
  if (n(";") > comma) return ";";
  if (n("|") > comma && n("|") > n(";")) return "|";
  return null;
}
/**
 * Delimited text as comma-separated CSV, which the server reads: fields
 * quoted where they need it, and a column whose every value is a number
 * written with a decimal comma (3,5) written with a point.
 */
export function toCommaCsv(text, sep) {
  const rows = parseDelimited(text, sep).filter((r) => r.length > 1 || r[0] !== "");
  const width = Math.max(...rows.map((r) => r.length));
  const decimal = Array.from({ length: width }, (_, i) => {
    const vs = rows.slice(1).map((r) => r[i]).filter((v) => v != null && v !== "");
    return vs.length > 0 && vs.every((v) => /^-?\d+(,\d+)?$/.test(v)) && vs.some((v) => v.includes(","));
  });
  const cell = (v, i, head) => { let x = v ?? ""; if (!head && decimal[i]) x = x.replace(",", "."); return /[",\r\n]/.test(x) ? `"${x.replace(/"/g, '""')}"` : x; };
  return rows.map((r, k) => r.map((v, i) => cell(v, i, k === 0)).join(",")).join("\n") + "\n";
}

/** The first rows of a file as objects, and its columns. */
async function sample(file, format, max = 200) {
  const text = await file.slice(0, 2 << 20).text();
  let objs = [], ragged = 0;
  if (format === "csv" || format === "tsv") {
    const rows = parseDelimited(text, format === "tsv" ? "\t" : ",", max + 2);
    const head = rows.shift() || [];
    if (file.size > 2 << 20) rows.pop();
    // Rows with more cells than the header lose the rest: said, not hidden.
    ragged = rows.filter((r) => r.length > head.length && r.slice(head.length).some((x) => x !== "")).length;
    objs = rows.filter((r) => r.length > 1 || r[0] !== "").slice(0, max).map((r) => Object.fromEntries(head.map((c, i) => [c, r[i] === "" || r[i] == null ? null : r[i]])));
  } else if (format === "jsonl") {
    for (const line of text.split("\n").slice(0, max)) { const l = line.trim(); if (!l) continue; try { objs.push(JSON.parse(l)); } catch { break; } }
  } else if (format === "json") {
    try { const all = JSON.parse(file.size <= 2 << 20 ? text : await file.text()); objs = (Array.isArray(all) ? all : [all]).slice(0, max); } catch (e) { throw new Error(`not JSON: ${e.message}`); }
  }
  const cols = [...new Set(objs.flatMap((o) => Object.keys(o || {})))];
  return { objs, cols, partial: file.size > 2 << 20, ragged };
}

const ISO = /^\d{4}-\d{2}-\d{2}([T ]\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})?)?$/;
/** The field a column's values read as: {type, multi?, dim?}. */
export function inferType(values, fromCsv) {
  const vs = values.filter((v) => v != null && v !== "");
  if (!vs.length) return { type: "keyword" };
  const num = (v) => typeof v === "number" || (fromCsv && /^-?\d+(\.\d+)?([eE][-+]?\d+)?$/.test(v));
  if (vs.every((v) => typeof v === "boolean" || (fromCsv && /^(true|false)$/i.test(v)))) return { type: "bool" };
  if (vs.every(num)) return { type: vs.every((v) => Number.isInteger(Number(v)) && !/\./.test(String(v))) ? "int" : "float" };
  if (vs.every((v) => typeof v === "string" && ISO.test(v))) return { type: "timestamp" };
  if (vs.every(Array.isArray)) {
    const flat = vs.flat();
    if (flat.length && flat.every((x) => typeof x === "number") && vs.every((v) => v.length === vs[0].length) && vs[0].length >= 4) return { type: "vector", dim: vs[0].length };
    return { type: flat.every((x) => Number.isInteger(x)) && flat.length ? "int" : "keyword", multi: true };
  }
  if (vs.every((v) => typeof v === "object")) return { type: "sparse" };
  const strs = vs.map(String);
  const avg = strs.reduce((a, s) => a + s.length, 0) / strs.length;
  const words = strs.reduce((a, s) => a + s.split(/\s+/).length, 0) / strs.length;
  return avg > 32 || words > 3 ? { type: "text" } : { type: "keyword" };
}

/** Whether the server kept a load or a stream in its audit log, read back
 * from it (a key that cannot read the log sees nothing). */
function audited({ sent, reason, route }) {
  const el = h("div", { class: "small muted row nw", style: { gap: "6px" } });
  if (!can("admin")) return el;
  api("/v1/admin/audit?" + new URLSearchParams({ kind: "change", limit: "30" })).then((a) => {
    const e = (a.entries || []).find((x) => x.ts_ms >= sent - 5000 && x.route === route);
    if (e) clear(el, icon("okc"), ` Kept in the audit log as a change by ${e.who}${e.reason ? ", with the reason" : reason ? " (without the reason)" : ""}. `, h("a", { href: "#/access" }, "Access and audit"));
    else clear(el, icon("info"), reason ? " This server did not keep it in its audit log: the reason went with the request only." : " This server did not keep it in its audit log.");
  }).catch(() => { /* a key that cannot read the audit log */ });
  return el;
}

function render(page) {
  const left = h("div", { class: "stack" });
  const right = h("div", { class: "stack" });
  page.append(h("div", { class: "imp" }, left, right));
  let ts = [];
  let stopSources = null;
  tables().then((list) => { ts = list; start(); exportCard(); streamsCard(); }).catch((e) => clear(left, failure(e, "The tables")));
  // A shard without a primary refuses the rows whose keys fall on it, while
  // the other shards write theirs: say so above the loader and the stream.
  const outage = h("div", { style: { marginBottom: "12px" } });
  const drawOutage = () => {
    const nw = reach().noWrites || [];
    clear(outage, nw.length ? h("div", { class: "banner bad dt-banner" }, icon("warn"), h("div", { class: "grow" },
      h("b", {}, nw.length > 1 ? `${nw.map((x) => "Shard " + x.shard).join(", ")} take no writes now` : `Shard ${nw[0].shard} ${nw[0].words} now`),
      h("div", { class: "small" }, `${nw[0].why ? nw[0].why.replace(/^./, (c) => c.toUpperCase()) + ". " : ""}A load or a stream sent now writes the rows of the other shards and fails on the rest; nothing undoes the rows written. Load once ${nw.length > 1 ? "they have" : "it has"} a primary.`),
      h("div", { class: "small" }, h("a", { href: `#/cluster?shard=${nw[0].shard}` }, "Open in Cluster")))) : null);
    outage.style.display = nw.length ? "" : "none";
  };
  drawOutage();
  page.prepend(outage);
  // A cluster that follows another refuses its clients' writes: say so
  // above the loader, and again when a write is refused for it.
  const fol = h("div");
  page.prepend(fol);
  followBanner(fol, "import").catch(() => {});
  const failed = (msg, kind, ms) => { toast(msg, kind, ms); const n = followsWho(msg); if (n) { clear(fol, writeThere(n, "import", { banner: true })); fol.scrollIntoView?.({ block: "nearest" }); } };
  const offCluster = bus.on("cluster", drawOutage);

  // ------------------------------------------------------------ loading a file
  function start() {
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Load a file"), h("span", { class: "sub" }, "CSV, TSV, JSON Lines, JSON, a SQLite database or a Parquet file")),
      h("div", { class: "bd" }, dropzone({ label: "Drop a file here, or click to choose one", sub: "Its first rows show here before anything is written", accept: ".csv,.tsv,.json,.jsonl,.ndjson,.sqlite,.sqlite3,.db,.parquet", multiple: false, onFiles: (fs) => open(fs[0]) }),
        h("div", { class: "small muted", style: { marginTop: "10px" } }, "A column goes to the field of its name; columns the table has no field for are skipped and counted. Lists in CSV are a|b|c or JSON; vectors are JSON arrays."))));
  }
  async function open(file) {
    const format = await sniff(file);
    if (format === "csv") {
      const sep = otherSeparator((await file.slice(0, 4096).text()).split(/\r?\n/)[0]);
      if (sep && file.size > 64 << 20) { toast(`${file.name} is separated by '${sep}': save it with commas first (this page reads such a file up to 64 MB)`, "bad", 9000); return; }
      if (sep) {
        file = new File([toCommaCsv(await file.text(), sep)], file.name, { type: "text/csv" });
        toast(`${file.name} is separated by '${sep}': read as such${sep === ";" ? ", and numbers with a decimal comma as numbers" : ""}`, "info", 6000);
      }
    }
    if (format === "sqlite") return sqlitePlan(file);
    if (format === "parquet") return parquetPlan(file);
    let s;
    try { s = await sample(file, format); } catch (e) { toast(`${file.name}: ${e.message}`, "bad", 8000); return; }
    if (!s.objs.length) { toast(`${file.name} has no rows`, "bad"); return; }
    const fromCsv = format === "csv" || format === "tsv";
    // A column named as prose (a title, a name, notes) is searched by its
    // words, short as it is: TEXT, not KEYWORD.
    const PROSE = /^(title|name|subject|headline|summary|notes?|description|comment|body|text|caption)$/i;
    const cols = s.cols.map((c) => {
      const t = inferType(s.objs.map((o) => o[c]), fromCsv);
      if (t.type === "keyword" && !t.multi && PROSE.test(c) && s.objs.some((o) => typeof o[c] === "string" && /\s/.test(o[c].trim()))) t.type = "text";
      return { name: c, ...t, use: true };
    });
    const keyGuess = cols.find((c) => /^(id|key|_id|pk)$/i.test(c.name) && (c.type === "keyword" || c.type === "int")) || cols.find((c) => (c.type === "keyword" || c.type === "int") && new Set(s.objs.map((o) => o[c.name])).size === s.objs.length);
    const overlap = (t) => (t.short?.fields || []).filter((f) => s.cols.includes(f.name)).length;
    const best = [...ts].sort((a, b) => overlap(b) - overlap(a))[0];
    // Where the rows go is asked, never assumed: the table that shares the
    // most columns is offered first.
    const fits = best && overlap(best) >= Math.max(1, s.cols.length / 2);
    const st = { mode: null, suggest: fits ? best.table : null, fit: fits ? overlap(best) : 0, table: best?.table, name: file.name.replace(/\.[^.]+$/, "").toLowerCase().replace(/[^a-z0-9_]+/g, "_").replace(/^(\d)/, "t_$1") || "imported", key: keyGuess?.name, cols, shardBy: null, clusterBy: null };
    drawLoad(file, format, s, st);
  }
  function drawLoad(file, format, s, st) {
    const target = tableOf(st.table);
    const box = h("div", { class: "stack" });
    const fileRow = h("div", { class: "imp-file" }, icon("import", "lg"), h("div", { class: "grow" }, h("b", {}, file.name), h("div", { class: "small muted" }, `${fmt.bytes(file.size)} · ${format.toUpperCase()} · ${fmt.plural(s.objs.length, "row")} read${s.partial ? " from the start of it" : ""} · ${fmt.plural(s.cols.length, "column")}`)),
      h("button", { class: "btn ghost", onclick: start }, icon("x"), "Another file"));
    const raggedNote = s.ragged ? h("div", { class: "notice small" }, icon("warn"), ` ${fmt.plural(s.ragged, "row")} of the first ${fmt.n(s.objs.length)} ${s.ragged === 1 ? "has" : "have"} more values than the header names: the extra values are not loaded. Is the file separated by another character, or a header missing?`) : null;
    const modeSeg = seg([["existing", "Into a table that exists"], ["new", "Into a new table"]], st.mode, (v) => { st.mode = v; if (v === "existing" && st.suggest) st.table = st.suggest; drawLoad(file, format, s, st); });
    const tableSel = h("select", { class: "select", onchange: (e) => { st.table = e.target.value; drawLoad(file, format, s, st); } }, ts.map((t) => h("option", { value: t.table, selected: t.table === st.table }, t.table)));
    const nameIn = h("input", { class: "input mono", value: st.name, oninput: (e) => { st.name = e.target.value.trim(); check(); } });
    // the columns and where each goes
    const fieldOf = (c) => target?.short?.fields?.find((f) => f.name === c.name);
    const mapRows = st.cols.map((c) => {
      const f = st.mode === "existing" ? fieldOf(c) : null;
      const typeSel = h("select", { class: "select sm", onchange: (e) => { c.type = e.target.value; if (!["keyword", "int"].includes(c.type)) delete c.multi; if (c.type === "vector" && !c.dim) c.dim = 384; check(); drawLoad(file, format, s, st); } }, Object.entries(TYPES).map(([k, v]) => h("option", { value: k, selected: k === c.type }, v.label)));
      return h("tr", {},
        h("td", {}, st.mode === "new" ? h("input", { type: "checkbox", checked: c.use, title: "Make a field of it", onchange: (e) => { c.use = e.target.checked; if (!c.use && (st.shardBy === c.name || st.clusterBy === c.name)) drawLoad(file, format, s, st); else check(); } }) : null),
        h("td", { class: "mono" }, c.name),
        h("td", {}, h("span", { class: "row nw", style: { gap: "6px" } }, typeBadge(c), typeMeta(c).label + (c.multi ? "[]" : "") + (c.dim ? `(${c.dim})` : ""))),
        st.mode === "existing"
          ? h("td", {}, f ? h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "pill ok" }, icon("okc"), f.name), h("span", { class: "small muted" }, typeMeta(f).label + (f.multi ? "[]" : ""))) : h("span", { class: "pill warn" }, icon("warn"), "skipped"))
          : st.mode === "new" ? h("td", {}, h("span", { class: "row nw", style: { gap: "8px" } }, typeSel, (c.type === "keyword" || c.type === "int") && !c.multi ? h("label", { class: "row nw small dim", style: { gap: "4px" } }, h("input", { type: "radio", name: "imp-key", checked: st.key === c.name, onchange: () => { st.key = c.name; if (st.shardBy === c.name) drawLoad(file, format, s, st); else check(); } }), "key") : null)) : h("td", {}));
    });
    // Where a new table's rows live (shard_by) and in what order each
    // segment keeps them (cluster_by): what the trial had to write in SQL.
    const shardable = (c) => c.use && (c.type === "keyword" || c.type === "int") && !c.multi && c.name !== st.key;
    const clusterable = (c) => c.use && (c.type === "keyword" || c.type === "int" || c.type === "timestamp") && !c.multi;
    if (st.shardBy && !st.cols.some((c) => c.name === st.shardBy && shardable(c))) st.shardBy = null;
    if (st.clusterBy && !st.cols.some((c) => c.name === st.clusterBy && clusterable(c))) st.clusterBy = null;
    const emptyIn = (name) => s.objs.filter((o) => o[name] == null || o[name] === "").length;
    const shardSel = h("select", { class: "select sm", "aria-label": "Shard by", onchange: (e) => { st.shardBy = e.target.value || null; if (st.shardBy && !st.clusterBy) st.clusterBy = st.shardBy; drawLoad(file, format, s, st); } },
      h("option", { value: "", selected: !st.shardBy }, st.key ? `the key, ${st.key}` : "the row's id"),
      st.cols.filter(shardable).map((c) => h("option", { value: c.name, selected: st.shardBy === c.name }, c.name)));
    const clusterSel = h("select", { class: "select sm", "aria-label": "Order each segment by", onchange: (e) => { st.clusterBy = e.target.value || null; drawLoad(file, format, s, st); } },
      h("option", { value: "", selected: !st.clusterBy }, "the order rows come in"),
      st.cols.filter(clusterable).map((c) => h("option", { value: c.name, selected: st.clusterBy === c.name }, c.name)));
    const shardEmpty = st.shardBy ? emptyIn(st.shardBy) : 0;
    // A single server has no shards (CCB-16): only the order of each
    // segment is asked there.
    const sharded = isCoordinator();
    if (!sharded) st.shardBy = null;
    const placeRow = st.mode === "new" ? h("div", { class: "stack", style: { gap: "6px" } },
      h("div", { class: "row", style: { gap: "16px" } },
        sharded ? h("label", { class: "row nw small dim", style: { gap: "6px" } }, "Shard by", shardSel) : null,
        h("label", { class: "row nw small dim", style: { gap: "6px" } }, "Order each segment by", clusterSel)),
      h("div", { class: "small muted" }, !sharded
        ? (st.clusterBy ? `Rows with the same ${st.clusterBy} sit together in each segment, so reading one ${st.clusterBy}'s rows reads a few blocks.` : "Pick a column you read rows by (a user, a time) to keep its rows together in each segment.")
        : st.shardBy
        ? `Rows with the same ${st.shardBy} live on one shard, so reading or joining one ${st.shardBy}'s rows asks one shard. ${st.shardBy} becomes NOT NULL.`
        : `Rows are spread over the shards by ${st.key ? "their key" : "their id"}. Pick a column you look rows up or join by (a user, an item) to keep its rows on one shard.`),
      shardEmpty ? h("div", { class: "error small" }, `${fmt.plural(shardEmpty, "row")} of the ${fmt.n(s.objs.length)} read here ${shardEmpty === 1 ? "has" : "have"} no ${st.shardBy}: a row needs one to be placed, and is refused without it.`) : null) : null;
    const missing = st.mode === "existing" && target ? (target.short.fields || []).filter((f) => !s.cols.includes(f.name) && !f.embed && !f.from && !f.derive && f.name !== target.short.key) : [];
    const keyMissing = st.mode === "existing" && target?.short?.key && !s.cols.includes(target.short.key);
    const checkBox = h("div");
    const go = h("button", { class: "btn primary" }, icon("import"), "Load the rows");
    // What loading does to rows the table holds: an import upserts, so a row
    // whose key exists replaces it. Counted over the rows read here.
    const replaced = h("div", { class: "small" });
    async function countReplaced() {
      const key = target?.short?.key;
      if (st.mode !== "existing" || !target || !key || !s.cols.includes(key)) { clear(replaced); return; }
      const keys = [...new Set(s.objs.map((o) => o[key]).filter((v) => v != null && v !== ""))].slice(0, 500);
      if (!keys.length) { clear(replaced); return; }
      const t = target.short.fields.find((f) => f.name === key);
      const lit = (v) => t?.type === "int" && /^-?\d+$/.test(String(v)) ? String(v) : `'${String(v).replace(/'/g, "''")}'`;
      clear(replaced, h("span", { class: "muted" }, "Counting the keys the table holds…"));
      try {
        const j = await api("/v1/query", { body: { sql: `SELECT COUNT(*) FROM ${quoteName(target.table)} WHERE ${quoteName(key)} IN (${keys.map(lit).join(", ")})` } });
        const n = j.rows[0][0];
        clear(replaced, h("div", { class: n ? "banner warn dt-banner" : "banner dt-done" }, icon(n ? "warn" : "info"), h("div", { class: "grow" },
          h("b", {}, n ? `${fmt.n(n)} of the ${fmt.plural(keys.length, "key")} read here are in ${target.table} already: those rows are replaced` : `None of the ${fmt.plural(keys.length, "key")} read here is in ${target.table}: every row is new`),
          h("div", { class: "small muted" }, `An import into a table that holds rows upserts: a row whose ${key} exists replaces the old row whole.${s.partial ? " Only the start of the file was read here, so the whole file may replace more." : ""}`))));
      } catch (e) { clear(replaced, h("span", { class: "muted" }, `Could not count the keys it replaces: ${e.message}`)); }
    }
    const progress = h("div", { class: "imp-prog hidden" }, h("div", { class: "bar" }, h("span")), h("span", { class: "small dim txt" }));
    let ddl = null;
    async function check() {
      if (!st.mode) { ddl = null; clear(checkBox); go.disabled = true; go.title = "Pick where the rows go first"; return; }
      go.title = "";
      if (st.mode !== "new") { ddl = null; clear(checkBox); go.disabled = !target || !can("write"); return; }
      const short = { table: st.name, key: st.key, fields: st.cols.filter((c) => c.use).map((c) => ({ name: c.name, type: c.type, ...(c.multi ? { multi: true } : {}), ...(c.type === "vector" ? { dim: c.dim || 384 } : {}), ...(c.name === st.shardBy ? { not_null: true } : {}) })) };
      if (!short.key) delete short.key;
      if (st.shardBy) short.shard_by = st.shardBy;
      if (st.clusterBy) short.cluster_by = st.clusterBy;
      try { const j = await api("/v1/schema/check", { body: short }); ddl = j.sql; clear(checkBox, h("details", { class: "more" }, h("summary", {}, "The table it makes"), sqlBlock(ddl))); go.disabled = ts.some((t) => t.table === st.name) || shardEmpty > 0; if (go.disabled) checkBox.prepend(h("div", { class: "error small" }, `${st.name} exists: pick it above, or name the new table something else.`)); }
      catch (e) { ddl = null; go.disabled = true; clear(checkBox, h("div", { class: "error small" }, e.message.replace(/^schema error: /, ""))); }
    }
    go.onclick = async () => {
      // On production, a load into a table that exists says why (sent with the import).
      let reason;
      if (st.mode === "existing" && isProduction()) {
        const ok = await confirm({ title: `Load ${file.name} into ${st.table}?`, ok: "Load the rows", reason: "Why: sent with the import, for the audit log",
          body: h("div", { class: "stack", style: { gap: "8px" } }, h("p", { class: "dim", style: { margin: 0 } }, `${fmt.bytes(file.size)} of ${format.toUpperCase()} into ${st.table} on ${whereTo()}, which counts as production. Rows whose key ${st.table} holds are replaced; there is no undo.`), replaced.cloneNode(true)) });
        if (!ok) return;
        reason = ok.reason;
      }
      go.disabled = true;
      let made = null;
      try {
        let name = st.table;
        if (st.mode === "new") { await api("/v1/sql", { body: { sql: ddl } }); made = st.name; invalidate(); ts = await tables(true); name = st.name; }
        const sent = Date.now();
        const j = await upload(`/v1/import?format=${format}&table=${encodeURIComponent(name)}`, file, CT[format], progress, reason);
        done(name, j, file, { sent, reason, route: "/v1/import" });
      } catch (e) {
        // A table this load made, which a bad row left empty (an import into
        // an empty table writes nothing when a row is bad), goes again: the
        // retry, with the column's type changed, makes it anew.
        let gone = false;
        if (made) {
          try {
            const left = (await api("/v1/query", { body: { sql: `SELECT COUNT(*) FROM ${quoteName(made)}` } })).rows?.[0]?.[0];
            if (left === 0) { await api("/v1/sql", { body: { sql: `DROP TABLE ${quoteName(made)}` }, reason: "an import into a new table failed: the empty table it made goes" }); gone = true; }
          } catch { /* left as it is: the message says so */ }
          invalidate(); ts = await tables(true).catch(() => ts);
        }
        failed(made ? `${e.message}. ${gone ? `Nothing was loaded, and the empty table ${made} was dropped: change the column's type and load again.` : `Table ${made} was made and holds what was loaded.`}` : e.message, "bad", 12000);
        go.disabled = false; progress.classList.add("hidden");
        if (gone) check();
      }
    };
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Load a file")), h("div", { class: "bd" }, box)));
    clear(box, fileRow, raggedNote,
      h("div", { class: "row" }, modeSeg, st.mode === "existing" ? tableSel : st.mode === "new" ? h("label", { class: "row nw small dim" }, "named", nameIn) : null),
      !st.mode ? h("div", { class: "small" }, h("b", {}, "Where do the rows go? "), h("span", { class: "muted" }, st.suggest ? `${st.suggest} has ${st.fit} of the file's ${s.cols.length} columns: pick “Into a table that exists” to load into it, or make a new table.` : "No table has most of the file's columns: a new table fits it best.")) : null,
      st.mode === "existing" && target ? h("div", { class: "small dim" }, `${fmt.plural(st.cols.filter((c) => fieldOf(c)).length, "column")} of ${st.cols.length} go to fields of ${target.table}.`,
        keyMissing ? h("span", { class: "error small", style: { marginLeft: "6px" } }, `The file has no ${target.short.key} column, the key: rows without one are refused.`) : null,
        missing.length ? h("span", { class: "muted" }, ` ${missing.map((f) => f.name).join(", ")} stay empty.`) : null) : null,
      h("div", { class: "tbl-wrap" }, h("table", { class: "t imp-map" }, h("thead", {}, h("tr", {}, h("th", {}, ""), h("th", {}, "column"), h("th", {}, "reads as"), h("th", {}, st.mode === "existing" ? "goes to" : st.mode === "new" ? "the field" : ""))), h("tbody", {}, mapRows))),
      placeRow,
      checkBox,
      replaced,
      h("div", { class: "row" }, go, !st.mode ? h("span", { class: "small muted" }, "Pick where the rows go first") : null, progress),
      h("details", { class: "more", open: true }, h("summary", {}, `The first rows (${fmt.n(Math.min(s.objs.length, 50))})`), h("div", { class: "tbl-wrap", style: { maxHeight: "300px" } }, grid({ columns: s.cols, rows: s.objs.slice(0, 50).map((o) => s.cols.map((c) => o[c] ?? null)) }, { max: 50 }))));
    check(); countReplaced();
  }
  /** Sends a file with a progress bar; resolves the JSON answer. */
  function upload(url, file, type, progress, reason) {
    return new Promise((resolve, reject) => {
      const x = new XMLHttpRequest();
      x.open("POST", at(url));
      x.setRequestHeader("content-type", type);
      for (const [k, v] of Object.entries(authHeader())) x.setRequestHeader(k, v);
      x.setRequestHeader("x-strata-client", "command-center");
      if (reason) x.setRequestHeader("x-strata-reason", safeHeader(reason));
      const bar = progress.querySelector(".bar span"), txt = progress.querySelector(".txt");
      progress.classList.remove("hidden");
      const t0 = performance.now();
      x.upload.onprogress = (e) => { if (!e.lengthComputable) return; bar.style.width = (e.loaded / e.total * 100) + "%"; txt.textContent = `${fmt.bytes(e.loaded)} of ${fmt.bytes(e.total)} · ${fmt.bytes(e.loaded / Math.max(0.001, (performance.now() - t0) / 1000))}/s`; };
      x.upload.onload = () => { bar.style.width = "100%"; txt.textContent = "Sent: the server is writing the rows…"; };
      x.onload = () => { let j = {}; try { j = JSON.parse(x.responseText); } catch { /* not JSON */ } if (x.status >= 200 && x.status < 300) resolve(j); else reject(new Error(j.error || `HTTP ${x.status}`)); };
      x.onerror = () => reject(new Error("The upload failed: the server closed the connection"));
      x.send(file);
    });
  }
  function done(name, j, file, sentWith) {
    const skipped = j.skipped_fields && (Array.isArray(j.skipped_fields) ? j.skipped_fields : Object.keys(j.skipped_fields));
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Load a file")), h("div", { class: "bd stack" },
      h("div", { class: "banner ok" }, icon("okc", "lg"), h("div", { class: "grow" }, h("div", { class: "big" }, `${fmt.plural(j.written ?? 0, "row")} written to ${name}`), h("div", { class: "small muted" }, `from ${file.name} in ${fmt.ms(j.took_ms)}${skipped?.length ? ` · columns skipped: ${skipped.join(", ")}` : ""}`))),
      sentWith ? audited(sentWith) : null,
      h("div", { class: "row" },
        h("a", { class: "btn primary", href: `#/table/${encodeURIComponent(name)}?tab=search` }, icon("search"), "Search it"),
        h("a", { class: "btn", href: "#/sql?q=" + encodeURIComponent(`SELECT * FROM ${quoteName(name)} LIMIT 20;`) }, icon("sql"), "Query it"),
        h("a", { class: "btn", href: "#/builder?table=" + encodeURIComponent(name) }, icon("builder"), "Build a query"),
        h("a", { class: "btn", href: "#/schema?table=" + encodeURIComponent(name) }, icon("schema"), "See it in the schema"),
        h("span", { class: "sep" }), h("button", { class: "btn ghost", onclick: start }, icon("import"), "Load another file")))));
    exportCard();
  }

  // ------------------------------------------------------------ a SQLite file
  async function sqlitePlan(file) {
    clear(left, h("section", { class: "card" }, h("div", { class: "bd" }, h("div", { class: "small muted" }, `Reading ${file.name}…`))));
    let plan;
    try { plan = await api("/v1/import?format=sqlite&plan=true", { body: file, type: CT.sqlite }); }
    catch (e) { failed(e.message, "bad", 9000); start(); return; }
    const pick = new Set(plan.tables.map((t) => t.source));
    const progress = h("div", { class: "imp-prog hidden" }, h("div", { class: "bar" }, h("span")), h("span", { class: "small dim txt" }));
    const go = h("button", { class: "btn primary" }, icon("import"), "Import the tables");
    // A plan of this server says whether each table exists, and may not count its rows.
    const hasRows = plan.tables.some((t) => t.rows != null);
    go.onclick = async () => {
      go.disabled = true;
      try { const j = await upload(`/v1/import?format=sqlite&tables=${encodeURIComponent([...pick].join(","))}`, file, CT.sqlite, progress); invalidate(); ts = await tables(true); done(j.tables.map((t) => t.table).join(", "), { written: j.written, took_ms: j.took_ms }, file); }
      catch (e) { failed(e.message, "bad", 9000); go.disabled = false; }
    };
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Import a SQLite database")), h("div", { class: "bd stack" },
      h("div", { class: "imp-file" }, icon("db", "lg"), h("div", { class: "grow" }, h("b", {}, file.name), h("div", { class: "small muted" }, `${fmt.bytes(file.size)} · SQLite · ${fmt.plural(plan.tables.length, "table")}`)), h("button", { class: "btn ghost", onclick: start }, icon("x"), "Another file")),
      h("div", { class: "tbl-wrap" }, h("table", { class: "t" }, h("thead", {}, h("tr", {}, h("th", {}, ""), h("th", {}, "SQLite table"), h("th", {}, "becomes"), hasRows ? h("th", { class: "num" }, "rows") : null, h("th", {}, "notes"))),
        h("tbody", {}, plan.tables.map((t) => h("tr", {},
          h("td", {}, h("input", { type: "checkbox", checked: true, onchange: (e) => { if (e.target.checked) pick.add(t.source); else pick.delete(t.source); go.disabled = !pick.size; } })),
          h("td", { class: "mono" }, t.source), h("td", {}, h("span", { class: "row nw", style: { gap: "6px" } }, h("b", {}, t.table), (t.created ?? !t.exists) ? h("span", { class: "badge accent" }, "new table") : h("span", { class: "badge" }, "exists"))),
          hasRows ? h("td", { class: "num" }, t.rows == null ? "–" : fmt.n(t.rows)) : null, h("td", { class: "small dim" }, (t.notes || []).join(" · "))))))),
      plan.skipped?.length ? h("div", { class: "small muted" }, `Skipped: ${plan.skipped.map((x) => typeof x === "string" ? x : x.source || JSON.stringify(x)).join(", ")}`) : null,
      h("div", { class: "row" }, go, progress))));
  }

  // ------------------------------------------------------------ a Parquet file
  // The server reads the file (a plan writes nothing) and says what it makes
  // of it: the table, each column's field and type, what it leaves out.
  async function parquetPlan(file, st = {}) {
    clear(left, h("section", { class: "card" }, h("div", { class: "bd" }, h("div", { class: "small muted" }, `Reading ${file.name}…`))));
    const stem = file.name.replace(/\..*$/, "").toLowerCase().replace(/[^a-z0-9_]+/g, "_").replace(/^(\d)/, "_$1") || "imported";
    const q = (extra) => { const p = new URLSearchParams({ format: "parquet", ...extra }); if (st.table) p.set("table", st.table); if (st.key != null) p.set("key", st.key); return p; };
    let plan;
    try { plan = await api("/v1/import?" + q({ plan: "true" }), { body: file, type: CT.parquet }); }
    catch (e) {
      // A file no export wrote names no table: the file's name does.
      if (!st.table && /\?table=/.test(e.message)) return parquetPlan(file, { ...st, table: stem });
      failed(e.message, "bad", 9000); start(); return;
    }
    st.table = plan.table;
    const progress = h("div", { class: "imp-prog hidden" }, h("div", { class: "bar" }, h("span")), h("span", { class: "small dim txt" }));
    const go = h("button", { class: "btn primary" }, icon("import"), plan.exists ? `Import into ${plan.table}` : `Create ${plan.table} and import`);
    go.onclick = async () => {
      go.disabled = true;
      try { const j = await upload("/v1/import?" + q({}), file, CT.parquet, progress); invalidate(); ts = await tables(true); done(j.table, { written: j.written, took_ms: j.took_ms, skipped_fields: j.skipped }, file); if (j.notes?.length) toast(j.notes.join(" · "), "", 9000); }
      catch (e) { failed(e.message, "bad", 9000); go.disabled = false; }
    };
    const names = h("datalist", { id: "imp-pq-tables" }, ts.map((t) => h("option", { value: t.table })));
    const name = h("input", { class: "input mono", value: plan.table, list: "imp-pq-tables", style: { width: "220px" }, onchange: (e) => parquetPlan(file, { ...st, table: e.target.value.trim() || undefined }) });
    const fields = plan.fields || [];
    const byField = Object.fromEntries(Object.entries(plan.columns || {}).map(([c, f]) => [f, c]));
    for (const [p, f] of Object.entries(plan.partitions || {})) byField[f] = `${p}=… (directory)`;
    const keySel = h("select", { class: "input", disabled: plan.exists, onchange: (e) => parquetPlan(file, { ...st, key: e.target.value }) },
      h("option", { value: "" }, "no key"), fields.filter((f) => f.type === "keyword" || f.type === "int").map((f) => h("option", { value: f.name, selected: plan.key === f.name }, f.name)));
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Import a Parquet file")), h("div", { class: "bd stack" },
      h("div", { class: "imp-file" }, icon("db", "lg"), h("div", { class: "grow" }, h("b", {}, file.name), h("div", { class: "small muted" }, `${fmt.bytes(file.size)} · Parquet · ${fmt.plural(plan.rows, "row")} · ${fmt.plural(Object.keys(plan.columns || {}).length, "column")} read`)), h("button", { class: "btn ghost", onclick: start }, icon("x"), "Another file")),
      h("div", { class: "row", style: { gap: "10px" } }, h("label", { class: "small dim" }, "Into table"), name, names, plan.exists ? h("span", { class: "badge" }, "exists: its schema stands") : h("span", { class: "badge accent" }, "new table"),
        h("span", { class: "sep" }), h("label", { class: "small dim" }, "key"), keySel),
      h("div", { class: "tbl-wrap" }, h("table", { class: "t" }, h("thead", {}, h("tr", {}, h("th", {}, "Parquet column"), h("th", {}, "field"), h("th", {}, "type"))),
        h("tbody", {}, fields.filter((f) => byField[f.name]).map((f) => h("tr", {}, h("td", { class: "mono" }, byField[f.name]), h("td", {}, h("b", {}, f.name), plan.key === f.name ? h("span", { class: "badge", style: { marginLeft: "6px" } }, "key") : null), h("td", {}, h("span", { class: "row nw", style: { gap: "6px" } }, typeBadge(f), h("span", { class: "small" }, f.type + (f.multi ? "[]" : ""))))))))),
      plan.skipped && Object.keys(plan.skipped).length ? h("div", { class: "small muted" }, "Left out: ", Object.entries(plan.skipped).map(([c, why]) => `${c} (${why})`).join(" · ")) : null,
      plan.notes?.length ? h("div", { class: "small muted" }, plan.notes.join(" · ")) : null,
      h("div", { class: "row" }, go, progress))));
  }

  // ------------------------------------------------------------ export
  function exportCard() {
    const pick = new Set(cached().map((t) => t.table));
    // How big the tables are now, where the server lists their sizes.
    const sizeNote = h("div", { class: "small muted" });
    async function estimate() {
      // The sizes are an admin's to read: another key's estimate is left out.
      if (!can("admin")) { clear(sizeNote); return; }
      try {
        const j = await api("/v1/index/tables");
        const list = (Array.isArray(j) ? j : j.tables || []).filter((t) => pick.has(t.table));
        const bytes = list.reduce((a, t) => a + (t.bytes || 0), 0), rows = list.reduce((a, t) => a + (t.docs || 0), 0);
        clear(sizeNote, list.length ? `The ${list.length === 1 ? "table holds" : "tables hold"} ${fmt.plural(rows, "row")}, ${fmt.bytes(bytes)} on disk${isCoordinator() ? " over every shard (one copy)" : ""}: the file is of that order, larger with full-text tables.` : "");
      } catch { clear(sizeNote); }
    }
    let fts = true, idx = true, format = "sqlite";
    const download = (b, name) => {
      const a = h("a", { href: URL.createObjectURL(b), download: name });
      document.body.append(a); a.click(); a.remove();
      setTimeout(() => URL.revokeObjectURL(a.href), 5000);
    };
    const go = h("button", { class: "btn primary", onclick: async () => {
      go.disabled = true;
      // Parquet holds a table a file: one download each.
      if (format === "parquet") {
        try {
          let bytes = 0;
          for (const t of pick) {
            const r = await api("/v1/export?" + new URLSearchParams({ format: "parquet", tables: t }), { blob: true });
            const b = await r.blob();
            bytes += b.size;
            download(b, `${t}.parquet`);
          }
          toast(`Exported ${fmt.plural(pick.size, "Parquet file")}, ${fmt.bytes(bytes)}`, "ok");
        } catch (e) { failed(e.message, "bad", 8000); }
        go.disabled = false;
        return;
      }
      try {
        const q = new URLSearchParams();
        if (pick.size !== cached().length) q.set("tables", [...pick].join(","));
        if (!fts) q.set("fts", "false");
        if (!idx) q.set("indexes", "false");
        const r = await api("/v1/export" + (q.toString() ? "?" + q : ""), { blob: true });
        const b = await r.blob();
        download(b, "chloedb-export.sqlite");
        toast(`Exported ${fmt.bytes(b.size)}`, "ok");
      } catch (e) { failed(e.message, "bad", 8000); }
      go.disabled = false;
    } }, icon("download"), "Download as SQLite");
    const sqliteOpts = h("div", { class: "stack", style: { gap: "10px" } },
      h("label", { class: "row nw small dim" }, h("input", { type: "checkbox", checked: true, onchange: (e) => { fts = e.target.checked; } }), "full-text tables (FTS5) for TEXT fields"),
      h("label", { class: "row nw small dim" }, h("input", { type: "checkbox", checked: true, onchange: (e) => { idx = e.target.checked; } }), "indexes for KEYWORD and number fields"));
    const hint = h("span", { class: "small muted" }, "Vectors go as float32 blobs, lists as JSON arrays.");
    const sub = h("span", { class: "sub" }, "the database as a SQLite file");
    const formatSeg = seg([["sqlite", "SQLite"], ["parquet", "Parquet"]], format, (v) => {
      format = v;
      sqliteOpts.classList.toggle("hidden", v === "parquet");
      go.lastChild.textContent = v === "parquet" ? "Download as Parquet" : "Download as SQLite";
      sub.textContent = v === "parquet" ? "a Parquet file a table, for Spark, DuckDB, polars and pandas" : "the database as a SQLite file";
      hint.textContent = v === "parquet" ? "Vectors go as lists of float32, timestamps in UTC; each file keeps its ChloeDB schema." : "Vectors go as float32 blobs, lists as JSON arrays.";
    });
    // A coordinator exports a table a file (Parquet, from every shard);
    // the whole database as one SQLite file runs on a shard's server.
    const coord = isCoordinator();
    if (coord) {
      format = "parquet";
      sqliteOpts.classList.add("hidden");
      go.lastChild.textContent = "Download as Parquet";
      sub.textContent = "a Parquet file a table, of every shard's rows";
      hint.textContent = "Vectors go as lists of float32, timestamps in UTC. The whole database as SQLite: export on each shard's server (chloedb-server export --db).";
    }
    gate(go, "admin");
    const card = h("section", { class: "card", id: "imp-export" }, h("div", { class: "hd" }, h("h3", {}, "Export"), sub),
      h("div", { class: "bd stack", style: { gap: "10px" } },
        coord ? null : formatSeg,
        h("div", { class: "row", style: { gap: "6px" } }, cached().map((t) => h("button", { class: "chip", "aria-pressed": "true", onclick: (e) => { if (pick.has(t.table)) pick.delete(t.table); else pick.add(t.table); e.currentTarget.setAttribute("aria-pressed", String(pick.has(t.table))); go.disabled = !pick.size || !can("admin"); estimate(); } }, icon("table"), t.table))),
        sqliteOpts,
        sizeNote,
        h("div", { class: "row" }, go, hint)));
    estimate();
    const old = right.querySelector("#imp-export");
    if (old) old.replaceWith(card); else right.prepend(card);
  }

  // ------------------------------------------------------------ streams
  function streamsCard() {
    const list = h("div");
    const log = h("pre", { class: "code imp-log hidden" });
    // Empty: a line sent here writes rows, and one with a source keeps that
    // source's offset for good. The placeholder shows the shape.
    const ta = h("textarea", { class: "input mono", rows: 4, spellcheck: "false", style: { width: "100%", fontSize: "12px" }, placeholder: '{"op": "upsert", "table": "events", "doc": {"id": "e1", ...}}\n{"source": {"name": "orders/0", "offset": 41}, "op": "delete", "table": "orders", "key": "o7"}', oninput: () => { send.disabled = !ta.value.trim() || !can("write"); } });
    const send = h("button", { class: "btn", disabled: true, onclick: async () => {
      const lines = ta.value.trim().split("\n").filter((l) => l.trim());
      let ops = [];
      try { ops = lines.map((l) => JSON.parse(l)); } catch (e) { toast(`Not a JSON line: ${e.message}`, "bad", 6000); return; }
      const tablesHit = [...new Set(ops.map((o) => o.table).filter(Boolean))];
      const sources = [...new Set(ops.map((o) => o.source?.name).filter(Boolean))];
      const ok = await confirm({ title: `Send ${fmt.plural(lines.length, "operation")}?`, ok: "Send them", reason: isProduction() ? "Why: sent with the stream, for the audit log" : false,
        body: h("div", { class: "stack", style: { gap: "8px" } }, h("p", { class: "dim", style: { margin: 0 } }, `They write ${tablesHit.length ? "to " + tablesHit.join(", ") : "rows"} on ${whereTo()}${isProduction() ? ", which counts as production" : ""}. There is no undo.`),
          sources.length ? h("p", { class: "small", style: { margin: 0 } }, `The server keeps the offset of ${sources.join(", ")} from now on: a consumer of that source resumes from it.`) : null) });
      if (!ok) return;
      send.disabled = true; log.classList.remove("hidden"); clear(log);
      try {
        const headers = { "content-type": "application/x-ndjson", "x-strata-client": "command-center", ...authHeader() };
        if (ok.reason) headers["x-strata-reason"] = safeHeader(ok.reason);
        const sent = Date.now();
        const r = await fetch(at("/v1/stream"), { method: "POST", headers, body: ta.value.trim() + "\n" });
        if (!r.ok) { let m = `HTTP ${r.status}`; try { const j = await r.json(); m = j.error || m; } catch { /* not JSON */ } throw new Error(m); }
        const rd = r.body.getReader(), dec = new TextDecoder();
        for (;;) { const { value, done } = await rd.read(); if (done) break; log.append(dec.decode(value, { stream: true })); }
        log.after(audited({ sent, reason: ok.reason, route: "/v1/stream" }));
        refresh();
      } catch (e) { log.append(String(e.message)); }
      send.disabled = !ta.value.trim();
    } }, icon("play"), "Send…");
    async function refresh() {
      try {
        const j = await api("/v1/sources");
        clear(list, j.sources?.length ? h("table", { class: "t" }, h("thead", {}, h("tr", {}, h("th", {}, "source"), h("th", { class: "num" }, "last offset"), h("th", {}, "written"))),
          h("tbody", {}, j.sources.map((x) => h("tr", {}, h("td", { class: "mono" }, x.name), h("td", { class: "num" }, fmt.n(x.offset)), h("td", { class: "small dim" }, x.updated ? fmt.ago(Date.parse(x.updated)) : "–")))))
          : h("div", { class: "small muted" }, "No stream has written here yet. A Kafka consumer (Python: chloedb.ingest.kafka) or POST /v1/stream names its source and offset, and resumes from the offset kept here."));
      } catch (e) { clear(list, failure(e, "The sources")); }
    }
    stopSources = every(5000, refresh);
    right.append(h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Streams"), h("span", { class: "sub" }, "each source's last offset written")),
      h("div", { class: "bd stack", style: { gap: "10px" } }, list,
        h("details", { class: "more" }, h("summary", {}, "Send operations as a stream (NDJSON, a line each)"), ta, h("div", { class: "row", style: { marginTop: "6px" } }, send, h("span", { class: "small muted" }, "Each line is written in order; the answer streams back a line per batch.")), log))));
  }

  return () => { stopSources?.(); offCluster?.(); };
}
