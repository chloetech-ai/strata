// Import and export: drop a file (CSV, TSV, JSON Lines, JSON or a SQLite
// database), see its first rows and the type each column reads as, load
// it into a table that exists or one made for it, and watch it upload.
// The database goes out as a SQLite file, and the stream sources show
// how far each event stream has been written, with a box to send one.

import { h, icon, clear, fmt, api, authHeader, toast, empty, seg, sqlBlock, dropzone, every, failure } from "../core.js";
import { tables, cached, invalidate, table as tableOf, typeBadge, typeMeta, TYPES } from "../catalog.js";
import { quoteName } from "../sqledit.js";
import { grid } from "../results.js";

export default { id: "import", title: "Import and export", icon: "import", group: "Data", keywords: "import upload load csv tsv json jsonl ndjson sqlite file export download stream kafka sources offsets", render };

const SQLITE = "SQLite format 3\u0000";
const CT = { csv: "text/csv", tsv: "text/tab-separated-values", jsonl: "application/x-ndjson", json: "application/json", sqlite: "application/vnd.sqlite3" };

/** The format of a file, from its first bytes and its name. */
export async function sniff(file) {
  const head = await file.slice(0, 4096).text();
  if (head.startsWith(SQLITE)) return "sqlite";
  const ext = (file.name.split(".").pop() || "").toLowerCase();
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

/** The first rows of a file as objects, and its columns. */
async function sample(file, format, max = 200) {
  const text = await file.slice(0, 2 << 20).text();
  let objs = [];
  if (format === "csv" || format === "tsv") {
    const rows = parseDelimited(text, format === "tsv" ? "\t" : ",", max + 2);
    const head = rows.shift() || [];
    if (file.size > 2 << 20) rows.pop();
    objs = rows.filter((r) => r.length > 1 || r[0] !== "").slice(0, max).map((r) => Object.fromEntries(head.map((c, i) => [c, r[i] === "" || r[i] == null ? null : r[i]])));
  } else if (format === "jsonl") {
    for (const line of text.split("\n").slice(0, max)) { const l = line.trim(); if (!l) continue; try { objs.push(JSON.parse(l)); } catch { break; } }
  } else if (format === "json") {
    try { const all = JSON.parse(file.size <= 2 << 20 ? text : await file.text()); objs = (Array.isArray(all) ? all : [all]).slice(0, max); } catch (e) { throw new Error(`not JSON: ${e.message}`); }
  }
  const cols = [...new Set(objs.flatMap((o) => Object.keys(o || {})))];
  return { objs, cols, partial: file.size > 2 << 20 };
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

function render(page) {
  const left = h("div", { class: "stack" });
  const right = h("div", { class: "stack" });
  page.append(h("div", { class: "imp" }, left, right));
  let ts = [];
  let stopSources = null;
  tables().then((list) => { ts = list; start(); exportCard(); streamsCard(); }).catch((e) => clear(left, failure(e, "The tables")));

  // ------------------------------------------------------------ loading a file
  function start() {
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Load a file"), h("span", { class: "sub" }, "CSV, TSV, JSON Lines, JSON or a SQLite database")),
      h("div", { class: "bd" }, dropzone({ label: "Drop a file here, or click to choose one", sub: "Its first rows show here before anything is written", accept: ".csv,.tsv,.json,.jsonl,.ndjson,.sqlite,.sqlite3,.db", multiple: false, onFiles: (fs) => open(fs[0]) }),
        h("div", { class: "small muted", style: { marginTop: "10px" } }, "A column goes to the field of its name; columns the table has no field for are skipped and counted. Lists in CSV are a|b|c or JSON; vectors are JSON arrays."))));
  }
  async function open(file) {
    const format = await sniff(file);
    if (format === "sqlite") return sqlitePlan(file);
    let s;
    try { s = await sample(file, format); } catch (e) { toast(`${file.name}: ${e.message}`, "bad", 8000); return; }
    if (!s.objs.length) { toast(`${file.name} has no rows`, "bad"); return; }
    const fromCsv = format === "csv" || format === "tsv";
    const cols = s.cols.map((c) => ({ name: c, ...inferType(s.objs.map((o) => o[c]), fromCsv), use: true }));
    const keyGuess = cols.find((c) => /^(id|key|_id|pk)$/i.test(c.name) && (c.type === "keyword" || c.type === "int")) || cols.find((c) => (c.type === "keyword" || c.type === "int") && new Set(s.objs.map((o) => o[c.name])).size === s.objs.length);
    const overlap = (t) => (t.short?.fields || []).filter((f) => s.cols.includes(f.name)).length;
    const best = [...ts].sort((a, b) => overlap(b) - overlap(a))[0];
    const st = { mode: best && overlap(best) >= Math.max(1, s.cols.length / 2) ? "existing" : "new", table: best?.table, name: file.name.replace(/\.[^.]+$/, "").toLowerCase().replace(/[^a-z0-9_]+/g, "_").replace(/^(\d)/, "t_$1") || "imported", key: keyGuess?.name, cols };
    drawLoad(file, format, s, st);
  }
  function drawLoad(file, format, s, st) {
    const target = tableOf(st.table);
    const box = h("div", { class: "stack" });
    const fileRow = h("div", { class: "imp-file" }, icon("import", "lg"), h("div", { class: "grow" }, h("b", {}, file.name), h("div", { class: "small muted" }, `${fmt.bytes(file.size)} · ${format.toUpperCase()} · ${fmt.plural(s.objs.length, "row")} read${s.partial ? " from the start of it" : ""} · ${fmt.plural(s.cols.length, "column")}`)),
      h("button", { class: "btn ghost", onclick: start }, icon("x"), "Another file"));
    const modeSeg = seg([["existing", "Into a table"], ["new", "Into a new table"]], st.mode, (v) => { st.mode = v; drawLoad(file, format, s, st); });
    const tableSel = h("select", { class: "select", onchange: (e) => { st.table = e.target.value; drawLoad(file, format, s, st); } }, ts.map((t) => h("option", { value: t.table, selected: t.table === st.table }, t.table)));
    const nameIn = h("input", { class: "input mono", value: st.name, oninput: (e) => { st.name = e.target.value.trim(); check(); } });
    // the columns and where each goes
    const fieldOf = (c) => target?.short?.fields?.find((f) => f.name === c.name);
    const mapRows = st.cols.map((c) => {
      const f = st.mode === "existing" ? fieldOf(c) : null;
      const typeSel = h("select", { class: "select sm", onchange: (e) => { c.type = e.target.value; if (!["keyword", "int"].includes(c.type)) delete c.multi; if (c.type === "vector" && !c.dim) c.dim = 384; check(); drawLoad(file, format, s, st); } }, Object.entries(TYPES).map(([k, v]) => h("option", { value: k, selected: k === c.type }, v.label)));
      return h("tr", {},
        h("td", {}, st.mode === "new" ? h("input", { type: "checkbox", checked: c.use, title: "Make a field of it", onchange: (e) => { c.use = e.target.checked; check(); } }) : null),
        h("td", { class: "mono" }, c.name),
        h("td", {}, h("span", { class: "row nw", style: { gap: "6px" } }, typeBadge(c), typeMeta(c).label + (c.multi ? "[]" : "") + (c.dim ? `(${c.dim})` : ""))),
        st.mode === "existing"
          ? h("td", {}, f ? h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "pill ok" }, icon("okc"), f.name), h("span", { class: "small muted" }, typeMeta(f).label + (f.multi ? "[]" : ""))) : h("span", { class: "pill warn" }, icon("warn"), "skipped"))
          : h("td", {}, h("span", { class: "row nw", style: { gap: "8px" } }, typeSel, (c.type === "keyword" || c.type === "int") && !c.multi ? h("label", { class: "row nw small dim", style: { gap: "4px" } }, h("input", { type: "radio", name: "imp-key", checked: st.key === c.name, onchange: () => { st.key = c.name; check(); } }), "key") : null)));
    });
    const missing = st.mode === "existing" && target ? (target.short.fields || []).filter((f) => !s.cols.includes(f.name) && !f.embed && !f.from && !f.derive && f.name !== target.short.key) : [];
    const keyMissing = st.mode === "existing" && target?.short?.key && !s.cols.includes(target.short.key);
    const checkBox = h("div");
    const go = h("button", { class: "btn primary" }, icon("import"), "Load the rows");
    const progress = h("div", { class: "imp-prog hidden" }, h("div", { class: "bar" }, h("span")), h("span", { class: "small dim txt" }));
    let ddl = null;
    async function check() {
      if (st.mode !== "new") { ddl = null; clear(checkBox); go.disabled = !target; return; }
      const short = { table: st.name, key: st.key, fields: st.cols.filter((c) => c.use).map((c) => ({ name: c.name, type: c.type, ...(c.multi ? { multi: true } : {}), ...(c.type === "vector" ? { dim: c.dim || 384 } : {}) })) };
      if (!short.key) delete short.key;
      try { const j = await api("/v1/schema/check", { body: short }); ddl = j.sql; clear(checkBox, h("details", { class: "more" }, h("summary", {}, "The table it makes"), sqlBlock(ddl))); go.disabled = ts.some((t) => t.table === st.name); if (go.disabled) checkBox.prepend(h("div", { class: "error small" }, `${st.name} exists: pick it above, or name the new table something else.`)); }
      catch (e) { ddl = null; go.disabled = true; clear(checkBox, h("div", { class: "error small" }, e.message.replace(/^schema error: /, ""))); }
    }
    go.onclick = async () => {
      go.disabled = true;
      try {
        let name = st.table;
        if (st.mode === "new") { await api("/v1/sql", { body: { sql: ddl } }); invalidate(); ts = await tables(true); name = st.name; }
        const j = await upload(`/v1/import?format=${format}&table=${encodeURIComponent(name)}`, file, CT[format], progress);
        done(name, j, file);
      } catch (e) { toast(e.message, "bad", 9000); go.disabled = false; progress.classList.add("hidden"); }
    };
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Load a file")), h("div", { class: "bd" }, box)));
    clear(box, fileRow,
      h("div", { class: "row" }, modeSeg, st.mode === "existing" ? tableSel : h("label", { class: "row nw small dim" }, "named", nameIn)),
      st.mode === "existing" && target ? h("div", { class: "small dim" }, `${fmt.plural(st.cols.filter((c) => fieldOf(c)).length, "column")} of ${st.cols.length} go to fields of ${target.table}.`,
        keyMissing ? h("span", { class: "error small", style: { marginLeft: "6px" } }, `The file has no ${target.short.key} column, the key: rows without one are refused.`) : null,
        missing.length ? h("span", { class: "muted" }, ` ${missing.map((f) => f.name).join(", ")} stay empty.`) : null) : null,
      h("div", { class: "tbl-wrap" }, h("table", { class: "t imp-map" }, h("thead", {}, h("tr", {}, h("th", {}, ""), h("th", {}, "column"), h("th", {}, "reads as"), h("th", {}, st.mode === "existing" ? "goes to" : "the field"))), h("tbody", {}, mapRows))),
      checkBox,
      h("div", { class: "row" }, go, progress),
      h("details", { class: "more", open: true }, h("summary", {}, `The first rows (${fmt.n(Math.min(s.objs.length, 50))})`), h("div", { class: "tbl-wrap", style: { maxHeight: "300px" } }, grid({ columns: s.cols, rows: s.objs.slice(0, 50).map((o) => s.cols.map((c) => o[c] ?? null)) }, { max: 50 }))));
    check();
  }
  /** Sends a file with a progress bar; resolves the JSON answer. */
  function upload(url, file, type, progress) {
    return new Promise((resolve, reject) => {
      const x = new XMLHttpRequest();
      x.open("POST", url);
      x.setRequestHeader("content-type", type);
      for (const [k, v] of Object.entries(authHeader())) x.setRequestHeader(k, v);
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
  function done(name, j, file) {
    const skipped = j.skipped_fields && (Array.isArray(j.skipped_fields) ? j.skipped_fields : Object.keys(j.skipped_fields));
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Load a file")), h("div", { class: "bd stack" },
      h("div", { class: "banner ok" }, icon("okc", "lg"), h("div", { class: "grow" }, h("div", { class: "big" }, `${fmt.plural(j.written ?? 0, "row")} written to ${name}`), h("div", { class: "small muted" }, `from ${file.name} in ${fmt.ms(j.took_ms)}${skipped?.length ? ` · columns skipped: ${skipped.join(", ")}` : ""}`))),
      h("div", { class: "row" },
        h("a", { class: "btn primary", href: "#/sql?q=" + encodeURIComponent(`SELECT * FROM ${quoteName(name)} LIMIT 20;`) }, icon("sql"), "Query it"),
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
    catch (e) { toast(e.message, "bad", 9000); start(); return; }
    const pick = new Set(plan.tables.map((t) => t.source));
    const progress = h("div", { class: "imp-prog hidden" }, h("div", { class: "bar" }, h("span")), h("span", { class: "small dim txt" }));
    const go = h("button", { class: "btn primary" }, icon("import"), "Import the tables");
    go.onclick = async () => {
      go.disabled = true;
      try { const j = await upload(`/v1/import?format=sqlite&tables=${encodeURIComponent([...pick].join(","))}`, file, CT.sqlite, progress); invalidate(); ts = await tables(true); done(j.tables.map((t) => t.table).join(", "), { written: j.written, took_ms: j.took_ms }, file); }
      catch (e) { toast(e.message, "bad", 9000); go.disabled = false; }
    };
    clear(left, h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Import a SQLite database")), h("div", { class: "bd stack" },
      h("div", { class: "imp-file" }, icon("db", "lg"), h("div", { class: "grow" }, h("b", {}, file.name), h("div", { class: "small muted" }, `${fmt.bytes(file.size)} · SQLite · ${fmt.plural(plan.tables.length, "table")}`)), h("button", { class: "btn ghost", onclick: start }, icon("x"), "Another file")),
      h("div", { class: "tbl-wrap" }, h("table", { class: "t" }, h("thead", {}, h("tr", {}, h("th", {}, ""), h("th", {}, "SQLite table"), h("th", {}, "becomes"), h("th", { class: "num" }, "rows"), h("th", {}, "notes"))),
        h("tbody", {}, plan.tables.map((t) => h("tr", {},
          h("td", {}, h("input", { type: "checkbox", checked: true, onchange: (e) => { if (e.target.checked) pick.add(t.source); else pick.delete(t.source); go.disabled = !pick.size; } })),
          h("td", { class: "mono" }, t.source), h("td", {}, h("span", { class: "row nw", style: { gap: "6px" } }, h("b", {}, t.table), t.created ? h("span", { class: "badge accent" }, "new table") : h("span", { class: "badge" }, "exists"))),
          h("td", { class: "num" }, fmt.n(t.rows)), h("td", { class: "small dim" }, (t.notes || []).join(" · "))))))),
      plan.skipped?.length ? h("div", { class: "small muted" }, `Skipped: ${plan.skipped.map((x) => typeof x === "string" ? x : x.source || JSON.stringify(x)).join(", ")}`) : null,
      h("div", { class: "row" }, go, progress))));
  }

  // ------------------------------------------------------------ export
  function exportCard() {
    const pick = new Set(cached().map((t) => t.table));
    let fts = true, idx = true;
    const go = h("button", { class: "btn primary", onclick: async () => {
      go.disabled = true;
      try {
        const q = new URLSearchParams();
        if (pick.size !== cached().length) q.set("tables", [...pick].join(","));
        if (!fts) q.set("fts", "false");
        if (!idx) q.set("indexes", "false");
        const r = await api("/v1/export" + (q.toString() ? "?" + q : ""), { blob: true });
        const b = await r.blob();
        const a = h("a", { href: URL.createObjectURL(b), download: "strata-export.sqlite" });
        document.body.append(a); a.click(); a.remove();
        setTimeout(() => URL.revokeObjectURL(a.href), 5000);
        toast(`Exported ${fmt.bytes(b.size)}`, "ok");
      } catch (e) { toast(e.message, "bad", 8000); }
      go.disabled = false;
    } }, icon("download"), "Download as SQLite");
    const card = h("section", { class: "card", id: "imp-export" }, h("div", { class: "hd" }, h("h3", {}, "Export"), h("span", { class: "sub" }, "the database as a SQLite file")),
      h("div", { class: "bd stack", style: { gap: "10px" } },
        h("div", { class: "row", style: { gap: "6px" } }, cached().map((t) => h("button", { class: "chip", "aria-pressed": "true", onclick: (e) => { if (pick.has(t.table)) pick.delete(t.table); else pick.add(t.table); e.currentTarget.setAttribute("aria-pressed", String(pick.has(t.table))); go.disabled = !pick.size; } }, icon("table"), t.table))),
        h("label", { class: "row nw small dim" }, h("input", { type: "checkbox", checked: true, onchange: (e) => { fts = e.target.checked; } }), "full-text tables (FTS5) for TEXT fields"),
        h("label", { class: "row nw small dim" }, h("input", { type: "checkbox", checked: true, onchange: (e) => { idx = e.target.checked; } }), "indexes for KEYWORD and number fields"),
        h("div", { class: "row" }, go, h("span", { class: "small muted" }, "Vectors go as JSON text, lists as JSON arrays."))));
    const old = right.querySelector("#imp-export");
    if (old) old.replaceWith(card); else right.prepend(card);
  }

  // ------------------------------------------------------------ streams
  function streamsCard() {
    const list = h("div");
    const log = h("pre", { class: "code imp-log hidden" });
    const first = cached()[0];
    const key = first?.short?.key;
    const example = first ? JSON.stringify({ source: { name: "ui-test/0", offset: 0 }, op: "upsert", table: first.table, doc: { [key || "id"]: "ui-test-1" } }) : "";
    const ta = h("textarea", { class: "input mono", rows: 4, spellcheck: "false", style: { width: "100%", fontSize: "12px" }, value: example, placeholder: '{"op": "insert", "table": "events", "doc": {...}}' });
    const send = h("button", { class: "btn", onclick: async () => {
      send.disabled = true; log.classList.remove("hidden"); clear(log);
      try {
        const r = await fetch("/v1/stream", { method: "POST", headers: { "content-type": "application/x-ndjson", ...authHeader() }, body: ta.value.trim() + "\n" });
        const rd = r.body.getReader(), dec = new TextDecoder();
        for (;;) { const { value, done } = await rd.read(); if (done) break; log.append(dec.decode(value, { stream: true })); }
        refresh();
      } catch (e) { log.append(String(e.message)); }
      send.disabled = false;
    } }, icon("play"), "Send");
    async function refresh() {
      try {
        const j = await api("/v1/sources");
        clear(list, j.sources?.length ? h("table", { class: "t" }, h("thead", {}, h("tr", {}, h("th", {}, "source"), h("th", { class: "num" }, "last offset"), h("th", {}, "written"))),
          h("tbody", {}, j.sources.map((x) => h("tr", {}, h("td", { class: "mono" }, x.name), h("td", { class: "num" }, fmt.n(x.offset)), h("td", { class: "small dim" }, x.updated ? fmt.ago(Date.parse(x.updated)) : "–")))))
          : h("div", { class: "small muted" }, "No stream has written here yet. A Kafka consumer (Python: strata.kafka) or POST /v1/stream names its source and offset, and resumes from the offset kept here."));
      } catch (e) { clear(list, failure(e, "The sources")); }
    }
    stopSources = every(5000, refresh);
    right.append(h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Streams"), h("span", { class: "sub" }, "each source's last offset written")),
      h("div", { class: "bd stack", style: { gap: "10px" } }, list,
        h("details", { class: "more" }, h("summary", {}, "Send operations as a stream (NDJSON, a line each)"), ta, h("div", { class: "row", style: { marginTop: "6px" } }, send, h("span", { class: "small muted" }, "Each line is written in order; the answer streams back a line per batch.")), log))));
  }

  return () => stopSources?.();
}
