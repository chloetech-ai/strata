// Logs: every server's lines, merged, over the top bar's time range. One
// search bar holds the whole search (level, server, shard, route, status,
// request, words) and lives in the URL with the range, so a link opens the
// same lines. Dense one-line rows, newest first, followed live or not; a
// detail pane with the line's fields and where to go next (its request on
// every server, the copy in Cluster, its statement, the lines around it);
// the messages grouped as patterns; a histogram by level with the events
// that changed things. The log level is set per server, with an expiry and
// a reason. Queries live in Queries > History; SQL over the store opens in
// the SQL workbench ("As SQL"). Without a log store, the latest lines in
// this server's memory are shown instead, followed live.

import { h, s, icon, clear, fmt, card, api, bus, every, toast, seg, debounce, copy, showTip, hideTip, drawer, table, pill, empty, modal, confirm, can, needs, copyName, findCopy, link, when, whenTitle, ApiError, failure, gate } from "../core.js";
import { state, timeRange, rangeParams, setWindow, setRange, isCoordinator } from "../data.js";
import { lit, tables as catalogTables } from "../catalog.js";
import { ownerOf } from "../owners.js";
import { quoteName } from "../sqledit.js";

export default { id: "logs", title: "Logs", icon: "logs", group: "Operate", keywords: "log lines errors warnings request id level debug search patterns live tail server copy shard route status", render, onParams };

const LV = ["error", "warn", "info", "debug", "trace"];
const LCOLOR = { error: "var(--crit)", warn: "var(--s4)", info: "var(--s1)", debug: "var(--muted)", trace: "var(--line-2)" };
const PAGE = 200;
/** L6: Patterns samples the newest lines of each group of levels apart, so that a server on debug does not crowd out the errors. */
const BUCKETS = [{ levels: ["error", "warn"], n: 1000, name: "errors and warnings" }, { levels: ["info"], n: 600, name: "info" }, { levels: ["debug", "trace"], n: 400, name: "debug" }];
const COLS = "ts, node, level, target, method, route, message, fields, request_id, status, ms, id";
const KEYS = { level: "level", server: "node", node: "node", shard: "shard", route: "route", module: "target", target: "target", status: "status", request: "request_id", method: "method" };
const TONE = { alert_fired: "bad", copy_down: "bad", majority_lost: "bad", primary_changed: "warn", contain: "warn", alert_resolved: "good", copy_up: "good", majority_back: "good" };
const MARK_KINDS = new Set(["rollout", "upgrade", "primary_changed", "copy_down", "copy_up", "majority_lost", "majority_back", "alert_fired", "alert_resolved", "log_level", "change", "note", "contain", "autoscale", "moves_started", "moves_finished", "shard_added", "shard_removed"]);

const iso = (ms) => new Date(ms).toISOString();
const URL_RE = /https?:\/\/[^\s)"',\\]+/g;
const msOf = (ts) => typeof ts === "string" ? Date.parse(ts) : (ts || 0) / 1000;
const clock = (ms) => { const d = new Date(ms); return d.toLocaleTimeString("en-GB", { hour12: false }) + "." + String(d.getMilliseconds()).padStart(3, "0"); };
/** The time of a line; the milliseconds drop on a phone. */
const clockEl = (ms) => { const c = clock(ms); return [c.slice(0, -4), h("span", { class: "al-ms" }, c.slice(-4))]; };
/** A level, its first letter alone on a phone. */
const lvEl = (lv, cls = "al-l-lv") => h("span", { class: `${cls} al-lv-${lv}`, title: lv }, lv.charAt(0), h("span", { class: "al-lv-rest" }, lv.slice(1)));
const objects = (j) => (j.rows || []).map((r) => Object.fromEntries(j.columns.map((c, i) => [c, r[i]])));
function parseJson(t) { if (!t || typeof t !== "string") return t || null; try { return JSON.parse(t); } catch { return t; } }
/** Values as Rust's Debug prints them read as values (A15: `Some(0.036)` is 0.036). */
const plain = (v) => typeof v === "string" ? v.replace(/^Some\((.*)\)$/, "$1").replace(/^None$/, "none") : v;
const tz = (() => { try { const z = Intl.DateTimeFormat().resolvedOptions().timeZone, off = -new Date().getTimezoneOffset(); const utc = `UTC${off >= 0 ? "+" : "-"}${Math.floor(Math.abs(off) / 60)}${off % 60 ? ":" + String(Math.abs(off) % 60).padStart(2, "0") : ""}`; return !z || /^(UTC|Etc\/UTC|GMT)$/.test(z) ? (off ? utc : "UTC") : `${z} (${utc})`; } catch { return "local time"; } })();
const tzShort = /\(([^)]+)\)$/.exec(tz)?.[1] || tz;

/** A query of the log store: every server's on a coordinator, merged. */
const ask = (sql, opts = {}) => api("/v1/logs/query", { body: { sql }, ...opts });

/** A message as a pattern: numbers, ids, URLs and quoted text become placeholders. */
export function pattern(message, fields) {
  const f = parseJson(fields);
  const extra = f && typeof f === "object" ? ["error", "status", "kind"].filter((k) => f[k] != null).map((k) => `${k}=${typeof f[k] === "string" ? plain(f[k]) : JSON.stringify(f[k])}`).join(" ") : "";
  return `${message || ""}${extra ? " " + extra : ""}`
    .replace(/https?:\/\/[^\s)"',]+/g, "<url>")
    .replace(/'(?:[^']|'')*'/g, "<str>")
    .replace(/\b[0-9a-f]{12,}\b/gi, "<id>")
    .replace(/\b\d+(?:\.\d+)?(?:ms|s|µs)?\b/g, "<n>")
    .replace(/\s+/g, " ").trim().slice(0, 240);
}

// ---------------------------------------------------------------- the search
/** The search's tokens: `level>=warn`, `server:s1-0`, `"a phrase"`, `word`. */
function tokens(q) {
  const out = [];
  const re = /(-?[a-z_]+(?::|>=|=)"[^"]*"|-?"[^"]*"|\S+)/gi;
  let m;
  while ((m = re.exec(q || ""))) out.push(m[1]);
  return out;
}
/** The search as filters: {level: {op, v}, node: [...], shard, route, ..., text}. */
function parse(q) {
  const f = { text: [] };
  for (const t of tokens(q)) {
    const m = /^(-?)([a-z_]+)(:|>=|=)(.*)$/i.exec(t);
    const key = m && KEYS[m[2].toLowerCase()];
    if (!key) { f.text.push(t); continue; }
    const v = m[4].replace(/^"(.*)"$/, "$1");
    if (key === "level") f.level = { ge: m[3] === ">=", v: v.toLowerCase() };
    else (f[key] ||= []).push({ v, not: m[1] === "-" });
  }
  return f;
}
const quote = (v) => /[\s"]/.test(v) ? `"${String(v).replace(/"/g, "")}"` : String(v);
const tokenLabel = (t) => {
  const m = /^(-?)([a-z_]+)(:|>=|=)(.*)$/i.exec(t);
  if (!m || !KEYS[m[2].toLowerCase()]) return t.startsWith("-") ? `not ${t.slice(1)}` : t;
  const k = m[2].toLowerCase(), v = m[4].replace(/^"(.*)"$/, "$1");
  return `${k === "node" ? "server" : k === "target" ? "module" : k} ${m[1] ? "≠" : m[3] === ">=" ? "≥" : "="} ${v}`;
};

let reroute = null;
function onParams(r) { reroute?.(r.params); }

/**
 * The tables a Logs page can read besides the servers' own lines: a
 * TIMESTAMP field and a text one (otel_logs, and the user's own events).
 * [{table, ts, body, level, levelNum, service}]
 */
export function logTables(ts) {
  const out = [];
  for (const t of ts || []) {
    const fs = t.short?.fields || [];
    const pick = (re, types) => fs.find((f) => types.includes(f.type) && re.test(f.name));
    const tsf = pick(/^(ts|timestamp|time|at|created(_at)?|logged_at|event_time)$/i, ["timestamp"]) || fs.find((f) => f.type === "timestamp");
    const body = pick(/^(body|message|msg|text|line|log)$/i, ["text"]) || fs.find((f) => f.type === "text");
    if (!tsf || !body || ownerOf(t.table)?.machinery) continue;
    out.push({ table: t.table, ts: tsf.name, body: body.name, level: pick(/^(severity|severity_text|level|log_level|lvl)$/i, ["keyword"])?.name || null, levelNum: pick(/^severity_number$/i, ["int"])?.name || null, service: pick(/^(service|service_name|app|host|source|logger)$/i, ["keyword"])?.name || null });
  }
  return out.sort((a, b) => (b.table === "otel_logs") - (a.table === "otel_logs"));
}

/** The source picker at the top of Logs: the servers' own lines, or a table's. */
function sourcePicker(current) {
  const sel = h("select", { class: "select sm", title: "Whose lines: the servers' own, or a table with a time and a message (otel_logs, your events)", "aria-label": "Lines of" }, h("option", { value: "" }, "The servers' own lines"));
  catalogTables().then((ts) => {
    // Your own tables first; an app's says whose it is (LIV-12).
    const all = logTables(ts).map((x) => ({ ...x, o: ownerOf(x.table) })).sort((a, b) => !!a.o - !!b.o);
    for (const x of all) sel.append(h("option", { value: x.table, selected: x.table === current }, `Table ${x.table}${x.o ? ` (${x.o.owner})` : ""}`));
    sel.value = current || "";
    if (sel.options.length < 2) sel.remove();
  }).catch(() => sel.remove());
  sel.onchange = () => { location.hash = sel.value ? link("logs", { source: sel.value }) : link("logs"); };
  return sel;
}

/** Logs from a table (T5): its lines over the top bar's window, by minute, searched and filtered, with an alert over the search. */
function tableLogs(page, r, name) {
  const body = h("div", { class: "stack" });
  const sub = h("p", {});
  page.append(h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Logs"), sub), h("div", { class: "tools" }, sourcePicker(name))), body);
  let src = null, alive = true, gen = 0;
  const input = h("input", { class: "input", placeholder: "Words that every line must hold: timeout refused", value: r.params.get("q") || "", style: { flex: "1", minWidth: "0" } });
  let level = r.params.get("level") || "all", service = r.params.get("service") || "";
  const levelBox = h("span"), svcBox = h("span"), histBox = h("div", { class: "tl-hist" }), linesBox = h("div"), sqlBox = h("div", { class: "row", style: { gap: "8px", flexWrap: "wrap" } });
  const keep = () => { const p = new URLSearchParams(location.hash.split("?")[1] || ""); p.set("source", name); for (const [k, v] of [["q", input.value.trim()], ["level", level === "all" ? "" : level], ["service", service]]) { if (v) p.set(k, v); else p.delete(k); } try { history.replaceState(null, "", location.pathname + location.search + "#/logs?" + p); } catch { /* old browser */ } };
  const tsLit = (ms) => lit(new Date(ms).toISOString());
  function where({ relative = false } = {}) {
    const w = [];
    const r0 = timeRange();
    w.push(relative ? `${quoteName(src.ts)} > :since` : `${quoteName(src.ts)} >= ${tsLit(r0.from)} AND ${quoteName(src.ts)} <= ${tsLit(r0.to)}`);
    const q = input.value.trim();
    if (q) w.push(`MATCH(${quoteName(src.body)}, ${lit(q)})`);
    if (level !== "all") {
      if (src.levelNum) w.push(`${quoteName(src.levelNum)} >= ${level === "error" ? 17 : 13}`);
      else if (src.level) w.push(`${quoteName(src.level)} IN (${(level === "error" ? ["ERROR", "error", "FATAL", "fatal", "CRITICAL", "critical"] : ["WARN", "warn", "WARNING", "warning", "ERROR", "error", "FATAL", "fatal", "CRITICAL", "critical"]).map(lit).join(", ")})`);
    }
    if (service && src.service) w.push(`${quoteName(src.service)} = ${lit(service)}`);
    return w.join(" AND ");
  }
  async function load() {
    const my = ++gen;
    keep();
    const r0 = timeRange();
    const span = r0.to - r0.from;
    const unit = span <= 3 * 3600e3 ? "minute" : span <= 3 * 86400e3 ? "hour" : "day";
    const t = quoteName(src.table), ts = quoteName(src.ts);
    const cols = [src.ts, src.level, src.service, src.body].filter(Boolean).map(quoteName).join(", ");
    const linesSql = `SELECT ${cols} FROM ${t} WHERE ${where()} ORDER BY ${ts} DESC LIMIT 300`;
    const histSql = `SELECT date_trunc('${unit}', ${ts}) AS at, COUNT(*) AS n FROM ${t} WHERE ${where()} GROUP BY at ORDER BY at`;
    linesBox.classList.add("loading");
    const [lines, hist] = await Promise.allSettled([api("/v1/query", { body: { sql: linesSql } }), api("/v1/query", { body: { sql: histSql } })]);
    if (my !== gen || !alive) return;
    linesBox.classList.remove("loading");
    if (hist.status === "fulfilled") {
      const rows = hist.value.rows || [];
      const max = Math.max(1, ...rows.map((x) => x[1]));
      const total = rows.reduce((a, x) => a + x[1], 0);
      clear(histBox, h("div", { class: "small muted", style: { marginBottom: "4px" } }, `${fmt.n(total)} lines, by ${unit}`), h("div", { class: "tl-bars" }, rows.map((x) => h("span", { title: `${when(Date.parse(x[0]), { sec: false })}: ${fmt.n(x[1])}`, style: { height: Math.max(2, (x[1] / max) * 60) + "px" } }))));
    } else clear(histBox, h("div", { class: "error" }, hist.reason.message));
    if (lines.status === "fulfilled") {
      const j = lines.value, ci = (n) => j.columns.indexOf(n);
      const iT = ci(src.ts), iL = src.level ? ci(src.level) : -1, iS = src.service ? ci(src.service) : -1, iB = ci(src.body);
      clear(linesBox, j.rows?.length ? h("div", { class: "tlog" }, j.rows.map((x) => h("div", { class: "tlog-r" },
        h("span", { class: "mono small muted nowrap", title: x[iT] }, when(Date.parse(x[iT]))),
        iL >= 0 ? h("span", { class: "tlog-l " + String(x[iL] || "").toLowerCase() }, x[iL] || "") : null,
        iS >= 0 ? h("span", { class: "small dim nowrap" }, x[iS] || "") : null,
        h("span", { class: "mono small tlog-b" }, String(x[iB] ?? ""))))) : empty("logs", "No lines", "A wider window, fewer words, or another level."));
    } else clear(linesBox, h("div", { class: "error" }, lines.reason.message));
    const alertSql = `SELECT COUNT(*) FROM ${t} WHERE ${where({ relative: true })}`;
    clear(sqlBox,
      h("a", { class: "btn sm", href: link("sql", { q: linesSql }) }, icon("sql"), "As SQL"),
      gate(h("a", { class: "btn sm", title: "An alert that counts these lines every 30 s over its window", href: `#/alerts?new=1&sql=${encodeURIComponent(alertSql)}&name=${encodeURIComponent(`Lines in ${src.table}${input.value.trim() ? ` with "${input.value.trim()}"` : ""}${level !== "all" ? `, ${level} or worse` : ""}`)}&threshold=0` }, icon("alerts"), "Alert on these lines"), "admin"));
  }
  catalogTables().then(async (ts) => {
    src = logTables(ts).find((x) => x.table === name);
    if (!src) { clear(body, card({ body: empty("logs", `${name} has no lines to show`, "A log table needs a TIMESTAMP field and a TEXT one.", h("a", { class: "btn", href: link("logs") }, "The servers' own lines")) })); return; }
    sub.textContent = `The lines of table ${src.table}, over the top bar's window: ${src.ts} for the time, ${src.body} for the message${src.level ? `, ${src.level} for the level` : ""}.`;
    clear(levelBox, seg([["all", "All"], ["warn", "Warn+"], ["error", "Errors"]], level, (v) => { level = v; load(); }, { "aria-label": "Level" }));
    if (src.service) {
      const sel = h("select", { class: "select sm", "aria-label": src.service }, h("option", { value: "" }, `every ${src.service}`));
      api("/v1/query", { body: { sql: `SELECT ${quoteName(src.service)}, COUNT(*) AS n FROM ${quoteName(src.table)} GROUP BY ${quoteName(src.service)} ORDER BY n DESC LIMIT 30` } }).then((j) => { for (const x of j.rows || []) if (x[0] != null) sel.append(h("option", { value: x[0], selected: x[0] === service }, `${x[0]} (${fmt.si(x[1])})`)); }).catch(() => {});
      sel.onchange = () => { service = sel.value; load(); };
      clear(svcBox, sel);
    }
    input.onkeydown = (e) => { if (e.key === "Enter") load(); };
    input.oninput = debounce(load, 400);
    clear(body, h("div", { class: "row nw", style: { gap: "8px", flexWrap: "wrap" } }, icon("search"), input, levelBox, svcBox), card({ body: histBox }), sqlBox, card({ flush: true, body: linesBox }));
    load();
  }).catch((e) => clear(body, failure(e, "The tables")));
  const offs = [bus.on("range", () => src && load())];
  return () => { alive = false; offs.forEach((f) => f()); };
}

function render(page, r) {
  if (r.params.get("source")) return tableLogs(page, r, r.params.get("source"));
  // Old links: the Queries tab is Queries > History, the SQL tab is the SQL workbench.
  const tab0 = r.params.get("tab");
  if (tab0 === "queries") { location.replace(link("queries", { tab: "history", request: r.params.get("request"), fingerprint: r.params.get("fingerprint") })); return; }
  if (tab0 === "sql" && r.params.get("sql")) { location.replace(link("sql", { q: r.params.get("sql"), store: "logs" })); return; }

  const ctx = { coord: false, nodes: new Map(), store: null, has: null, missed: [] };
  page.classList.add("al-logs");
  const sub = h("p", { class: "al-lsub" });
  const storeBtn = h("button", { class: "btn sm", title: "Each server's log store: size, retention, writes" }, icon("db"), "stores");
  const levelBtn = h("button", { class: "btn sm", title: "What each server logs, with an expiry" }, icon("sliders"), "Log levels");
  const sqlLink = h("a", { class: "btn sm ghost al-wide-only", title: "Open this search as SQL in the SQL workbench" }, icon("sql"), "As SQL");
  const shareBtn = h("button", { class: "btn sm ghost al-wide-only", title: "Copy a link to these lines: the search and this time window" }, icon("link"), "Copy link");
  // NEW-alerts-logs-15: on a phone the chart and the secondary links fold away, so lines come first.
  const foldBtn = (label, cls, title) => { const b = h("button", { class: "chip al-narrow-only", "aria-pressed": "false", title, onclick: () => { const on = page.classList.toggle(cls); b.setAttribute("aria-pressed", String(on)); } }, label); return b; };
  const chartBtn = foldBtn("Chart", "al-chart-open", "The lines by level and minute, and the events");
  const moreBtn = foldBtn("More", "al-more-open", "As SQL, Copy link, Queries > History");
  const chips = h("div", { class: "al-sq-chips" });
  const input = h("input", { class: "al-sq-in", placeholder: "Search: words, \"a phrase\", level>=warn, server:s1-0, shard:1, route:, status:, request:", "aria-label": "Search the log lines", autocomplete: "off", spellcheck: "false" });
  const bar = h("div", { class: "al-sq", onclick: (e) => { if (e.target === bar || e.target === chips) input.focus(); } }, icon("search"), chips, input);
  const rangeNote = h("span", { class: "small muted" });
  const alertChip = h("span");
  const liveBtn = h("button", { class: "chip al-live", "aria-pressed": "true", title: "Add each new line at the top as it is written" });
  const missedLine = h("div");
  const hist = h("div", { class: "hist" });
  const markLegend = h("div", { class: "al-marks small muted" });
  const facets = h("div", { class: "al-facets" });
  const tabs = h("div", { class: "tabs al-ltabs", role: "tablist" });
  const countNote = h("span", { class: "small muted" });
  const list = h("div", { class: "al-lines", role: "grid", "aria-label": "Log lines, newest first" });
  const more = h("div", { class: "al-more" });
  const detail = h("aside", { class: "al-detail card hidden", "aria-label": "The line's detail" });
  const scrim = h("div", { class: "al-scrim", onclick: () => closeDetail() });
  const filtersBtn = h("button", { class: "chip al-narrow-only al-fbtn", "aria-pressed": "false", onclick: () => { const on = main.classList.toggle("al-ffo"); filtersBtn.setAttribute("aria-pressed", String(on)); } }, icon("filter"), "Filters");
  const main = h("div", { class: "al-lmain" }, facets, h("div", { class: "al-lcol" }, h("div", { class: "row nw al-lhead" }, tabs, h("span", { class: "sep" }), countNote), list, more), scrim, detail);
  page.append(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Logs"), sub),
      h("div", { class: "tools" }, sourcePicker(""), storeBtn, levelBtn, h("a", { class: "btn sm ghost al-wide-only", href: link("queries", { tab: "history" }), title: "Finished queries, failed and slow ones, with their traces: Queries > History" }, icon("queries"), "Queries > History"))),
    h("div", { class: "row nw al-sqrow" }, bar, sqlLink, shareBtn),
    h("div", { class: "row al-rangerow" }, alertChip, rangeNote, h("span", { class: "sep" }), filtersBtn, chartBtn, moreBtn, liveBtn),
    h("section", { class: "card al-hist" }, h("div", { class: "bd" }, hist, markLegend)),
    missedLine,
    main);

  let q = "", tab = "lines", live = true, gen = 0, newest = null, oldest = null, matched = 0, rows = [], sel = null, sample = null, buckets = [];
  const f = () => parse(q);

  // ------------------------------------------------ servers
  /** A server's name: the copy's (s1-0 or its node name), else the node name the store gives. */
  const nameOf = (node) => { const n = ctx.nodes.get(node); return n?.url && findCopy(n.url) ? copyName(n.url) : node; };
  const urlOf = (node) => ctx.nodes.get(node)?.url || null;
  /** The node names a server token means: a node name, a copy's name or URL. */
  function nodesFor(v) {
    const out = [];
    for (const [node, n] of ctx.nodes) if (node === v || (n.url && (n.url === v || copyName(n.url) === v))) out.push(node);
    return out.length ? out : [v];
  }
  const shardNodes = (sh) => [...ctx.nodes].filter(([, n]) => String(n.shard) === String(sh)).map(([k]) => k);
  /** Copy URLs in a text, named: "shard 17 (http://…:7701)" reads "shard 17 (s17-0)". */
  const nameUrls = (t) => String(t ?? "").replace(URL_RE, (u) => { const tail = /[:;.]+$/.exec(u)?.[0] || "", b = tail ? u.slice(0, -tail.length) : u; return (findCopy(b) ? copyName(b) : b) + tail; });

  // ------------------------------------------------ SQL
  function where(skip) {
    const x = f(), t = timeRange();
    const out = [`ts >= ${lit(iso(t.from))}`];
    if (!(t.live && live)) out.push(`ts < ${lit(iso(t.to))}`);
    if (x.level && skip !== "level") out.push(x.level.ge ? `level IN (${LV.slice(0, Math.max(0, LV.indexOf(x.level.v)) + 1).map(lit).join(", ")})` : `level = ${lit(x.level.v)}`);
    const nodeIn = [];
    for (const { v, not } of x.node || []) if (!not) nodeIn.push(...nodesFor(v)); else out.push(`node != ${lit(nodesFor(v)[0])}`);
    for (const { v, not } of x.shard || []) if (!not) nodeIn.push(...shardNodes(v)); else for (const n of shardNodes(v)) out.push(`node != ${lit(n)}`);
    if (nodeIn.length && skip !== "node") out.push(`node IN (${[...new Set(nodeIn)].map(lit).join(", ")})`);
    else if ((x.node?.some((y) => !y.not) || x.shard?.some((y) => !y.not)) && !nodeIn.length && skip !== "node") out.push("node = ''");
    for (const k of ["route", "target", "method", "request_id"]) for (const { v, not } of x[k] || []) if (skip !== k) out.push(`${k} ${not ? "!=" : "="} ${lit(v)}`);
    for (const { v, not } of x.status || []) if (skip !== "status" && Number.isFinite(+v)) out.push(`status ${not ? "!=" : "="} ${+v}`);
    if (x.text.length) {
      const words = x.text.join(" ");
      out.push(`(MATCH(message, ${lit(words)}) OR MATCH(fields, ${lit(words)}) OR MATCH(error, ${lit(words)}))`);
    }
    return out.join(" AND ");
  }
  const rowsSql = (extra = "") => `SELECT ${COLS} FROM logs WHERE ${where()}${extra} ORDER BY ts DESC, id DESC LIMIT ${PAGE}`;

  // ------------------------------------------------ the search bar
  function setQ(next, { push = true } = {}) {
    q = next.trim().replace(/\s+/g, " ");
    drawChips();
    if (push) writeUrl();
    refresh();
  }
  function writeUrl() {
    const want = link("logs", { q, tab: tab === "lines" ? null : tab, alert: alertId });
    if (location.hash !== want) history.replaceState(null, "", location.pathname + location.search + want);
    sqlLink.href = link("sql", { q: rowsSql(), store: "logs" });
  }
  function drawChips() {
    clear(chips, tokens(q).map((t, i) => h("button", { class: "chip al-tok", "aria-pressed": "true", title: "Remove (click the text to edit it)", onclick: (e) => {
      const all = tokens(q);
      all.splice(i, 1);
      if (!e.target.closest(".x")) { input.value = t; input.focus(); }
      setQ(all.join(" "));
    } }, tokenLabel(t), h("span", { class: "x", "aria-hidden": "true" }, "×"))));
  }
  const addTok = (t) => { const all = tokens(q).filter((x) => x !== t); all.push(t); setQ(all.join(" ")); };
  const dropKey = (key) => tokens(q).filter((t) => { const m = /^-?([a-z_]+)(:|>=|=)/i.exec(t); return !(m && KEYS[m[1].toLowerCase()] === key); });
  const setKey = (key, t) => { const all = dropKey(key); if (t) all.push(t); setQ(all.join(" ")); };
  input.onkeydown = (e) => {
    if (e.key === "Enter") { e.preventDefault(); const v = input.value.trim(); input.value = ""; if (v) setQ(`${q} ${v}`); }
    else if (e.key === "Backspace" && !input.value && q) { const all = tokens(q); input.value = all.pop(); e.preventDefault(); setQ(all.join(" ")); }
    else if (e.key === "Escape" && input.value) { input.value = ""; }
  };
  input.onblur = () => { const v = input.value.trim(); if (v) { input.value = ""; setQ(`${q} ${v}`); } };

  // ------------------------------------------------ time and live
  let alertId = r.params.get("alert") || null;
  function drawRange() {
    const t = timeRange();
    clear(rangeNote, t.fixed ? `${when(t.from)} – ${when(t.to)}` : t.label, h("span", { class: "al-wide-only" }, " · the top bar's range, kept in the link"), ` · times in ${tz}`);
    liveBtn.disabled = !t.live;
    liveBtn.title = t.live ? "Add each new line at the top as it is written" : "The range is a fixed window: pick a range like 1h in the top bar to follow new lines";
    liveBtn.setAttribute("aria-pressed", String(live && t.live));
    clear(liveBtn, h("span", { class: "dot " + (live && t.live ? "ok pulse" : "") }), live && t.live ? "Live: new lines on top" : "Live: off");
    const a = alertId && state.alerts ? (state.alerts.rules || []).find((x) => x.id === alertId) : null;
    clear(alertChip, alertId ? h("button", { class: "chip al-alertchip", "aria-pressed": "true", title: "Lines of the alert's window: remove to go back to the last hour", onclick: () => { alertId = null; setRange("1h"); } },
      icon("alerts"), `Alert window: ${a?.name || alertId}`, t.fixed ? ` · ${when(t.from)} – ${when(t.to)}` : "", h("span", { class: "x" }, "×")) : null);
  }
  liveBtn.onclick = () => { live = !live; drawRange(); if (live) refresh(); };

  // ------------------------------------------------ the histogram
  let events = [];
  async function loadEvents() {
    const t = timeRange();
    try {
      const j = await api(`/v1/events?from=${Math.round(t.from)}&to=${Math.round(t.to)}&limit=300`);
      events = (j.events || []).filter((e) => MARK_KINDS.has(e.kind) && !(e.kind === "change" && /^\/v1\/alerts\/[^/]+\/(ack|silence|notes)$/.test(e.detail?.route || "")));
      ctx.newer = true;
    } catch (e) { events = []; if (e instanceof ApiError && (e.status === 404 || e.status === 405)) ctx.newer = false; }
  }
  /** The levels the search keeps. */
  function levelOk() {
    const x = f().level;
    if (!x) return () => true;
    const i = LV.indexOf(x.v);
    return x.ge ? (k) => LV.indexOf(k) >= 0 && LV.indexOf(k) <= i : (k) => k === x.v;
  }
  let levelTotals = {};
  const histSql = (col) => `SELECT ${col}, level, COUNT(*) FROM logs WHERE ${where("level")} GROUP BY ${col}, level LIMIT 20000`;
  function drawHist(hrows, unit) {
    const t = timeRange();
    const to = t.to, spanMin = Math.max(1, (to - t.from) / 60e3);
    const binMin = Math.max(unit, Math.ceil(spanMin / 120 / unit) * unit);
    const t0 = Math.floor(t.from / 60e3 / binMin) * binMin, n = Math.max(1, Math.ceil((to / 60e3 - t0) / binMin));
    const bins = Array.from({ length: n }, () => ({}));
    const totals = {};
    for (const [b, k, c] of hrows) {
      if (b == null) continue;
      const i = Math.floor((b * unit - t0) / binMin);
      if (i < 0 || i >= n) continue;
      const key = k ?? "–";
      bins[i][key] = (bins[i][key] || 0) + c;
      totals[key] = (totals[key] || 0) + c;
    }
    const ok = levelOk();
    const keys = [...LV.filter((k) => totals[k]), ...Object.keys(totals).filter((k) => !LV.includes(k))].filter(ok);
    const max = Math.max(1, ...bins.map((b) => keys.reduce((a, k) => a + (b[k] || 0), 0)));
    const W = 1000, H = 64, bw = W / n, binMs = binMin * 60e3, tAt = (i) => (t0 + i * binMin) * 60e3;
    const X = (ms) => (ms / 60e3 - t0) / (n * binMin) * W;
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, style: { width: "100%", height: H + "px", cursor: "crosshair", touchAction: "pan-y" }, preserveAspectRatio: "none", role: "img", "aria-label": "Log lines over time, by level" });
    bins.forEach((b, i) => {
      let y = H;
      for (const k of keys) {
        const v = b[k] || 0; if (!v) continue;
        const hh = Math.max(1, v / max * (H - 4));
        y -= hh;
        const rc = s("rect", { x: i * bw + 0.5, y, width: Math.max(1, bw - 1), height: hh, style: { fill: LCOLOR[k] || "var(--s6)" } });
        rc.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `${when(tAt(i))} – ${when(tAt(i + 1))}`), ...keys.filter((x) => b[x]).map((x) => h("div", { class: "r" }, h("span", { class: "k box", style: { background: LCOLOR[x] } }), h("b", {}, fmt.n(b[x])), h("span", { class: "n" }, x)))]));
        rc.addEventListener("pointerleave", hideTip);
        svgEl.append(rc);
      }
    });
    for (const e of events) {
      if (e.ts_ms < t.from || e.ts_ms > to) continue;
      const x = X(e.ts_ms);
      const ln = s("line", { class: "al-hmk " + (TONE[e.kind] || ""), x1: x, x2: x, y1: 0, y2: H, "vector-effect": "non-scaling-stroke" });
      ln.addEventListener("pointermove", (ev) => showTip(ev.clientX, ev.clientY, [h("div", { class: "t" }, when(e.ts_ms)), h("div", {}, e.title || e.kind)]));
      ln.addEventListener("pointerleave", hideTip);
      svgEl.append(ln);
    }
    const axis = h("div", { class: "al-hax", "aria-hidden": "true" }, [0, 1, 2, 3, 4].map((k) => h("span", { class: k % 2 ? "al-hax-odd" : "" }, when(tAt(0) + k / 4 * n * binMs, { sec: false }))));
    const selr = s("rect", { class: "selr", x: 0, y: 0, width: 0, height: H, style: { display: "none" } });
    svgEl.append(selr);
    const xOf = (e) => { const rr = svgEl.getBoundingClientRect(); return Math.max(0, Math.min(1, (e.clientX - rr.left) / rr.width)); };
    svgEl.addEventListener("pointerdown", (e) => {
      const a = xOf(e);
      svgEl.setPointerCapture(e.pointerId);
      const mv = (ev) => { const b = xOf(ev); selr.style.display = ""; selr.setAttribute("x", Math.min(a, b) * W); selr.setAttribute("width", Math.abs(b - a) * W); };
      const up = (ev) => {
        svgEl.removeEventListener("pointermove", mv); svgEl.removeEventListener("pointerup", up);
        selr.style.display = "none"; hideTip();
        const b = xOf(ev);
        if (Math.abs(b - a) * svgEl.getBoundingClientRect().width < 5) { const i = Math.min(n - 1, Math.floor(a * n)); if (bins[i] && Object.keys(bins[i]).length) setWindow(tAt(i), tAt(i + 1)); }
        else setWindow(tAt(0) + Math.min(a, b) * n * binMs, tAt(0) + Math.max(a, b) * n * binMs);
      };
      svgEl.addEventListener("pointermove", mv); svgEl.addEventListener("pointerup", up);
    });
    const total = keys.reduce((a, k) => a + totals[k], 0);
    levelTotals = totals;
    clear(hist, svgEl, axis, h("div", { class: "legend", style: { marginTop: "4px" } },
      keys.map((k) => h("button", { class: "row nw al-lvkey", title: `Only ${k}`, onclick: () => setKey("level", `level:${k}`) }, h("span", { class: "k box", style: { background: LCOLOR[k] } }), k, h("b", { class: "num" }, fmt.si(totals[k])))),
      h("span", { class: "sep" }), h("span", { class: "muted" }, `a bar is ${binMin >= 60 ? fmt.plural(binMin / 60, "hour") : fmt.plural(binMin, "minute")} · drag to zoom`)));
    clear(markLegend, events.filter((e) => e.ts_ms >= t.from && e.ts_ms <= to).sort((a, b) => b.ts_ms - a.ts_ms).slice(0, 6).map((e) => h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "al-mkey " + (TONE[e.kind] || "") }), `${when(e.ts_ms, { sec: false })} ${e.title || e.kind}`)));
    return total;
  }

  // ------------------------------------------------ facets
  function facet(title, items, active, onPick, fmtv = (v) => v) {
    const max = Math.max(1, ...items.map((x) => x[1]));
    return h("section", { class: "card al-facet" }, h("div", { class: "hd" }, h("h3", {}, title)), h("div", { class: "bd" },
      items.length ? h("div", { class: "facet" }, items.map(([v, n, dot]) => h("button", { "aria-pressed": String(active === String(v)), title: active === String(v) ? "Show them all again" : `Only ${fmtv(v)}`, onclick: () => onPick(active === String(v) ? null : v) },
        h("span", { class: "ellipsis mono" }, dot ? h("span", { class: "dot " + dot, style: { marginRight: "6px" } }) : null, fmtv(v)), h("b", { class: "num" }, fmt.si(n)),
        h("div", { class: "meter" }, h("i", { style: { width: n / max * 100 + "%" } }))))) : h("div", { class: "small muted" }, "None")));
  }
  function drawFacets(lv, nodes, status, routes) {
    const x = f();
    const shardCount = {};
    for (const [node, n] of nodes) { const sh = ctx.nodes.get(node)?.shard; if (sh != null) shardCount[sh] = (shardCount[sh] || 0) + n; }
    const downShards = new Set(ctx.missed.map((m) => String(m.shard)));
    const one = (key) => (x[key] || []).filter((y) => !y.not).map((y) => y.v)[0] ?? null;
    clear(facets,
      facet("Level", LV.map((l) => [l, (lv.find((y) => y[0] === l) || [])[1] || 0]).filter((y) => y[1]), x.level && !x.level.ge ? x.level.v : null, (v) => setKey("level", v ? `level:${v}` : null)),
      ctx.coord && Object.keys(shardCount).length ? facet("Shard", Object.entries(shardCount).sort((a, b) => b[1] - a[1]).slice(0, 8).map(([k, n]) => [k, n, downShards.has(k) ? "bad" : ""]), one("shard"), (v) => setKey("shard", v != null ? `shard:${v}` : null), (v) => `shard ${v}`) : null,
      ctx.coord ? facet("Server", nodes.slice(0, 10).map(([k, n]) => [nameOf(k), n, ctx.missed.some((m) => m.node === k) ? "bad" : ""]), one("node"), (v) => setKey("node", v ? `server:${v}` : null)) : null,
      facet("Status", status.filter((y) => y[0] != null).slice(0, 6), one("status"), (v) => setKey("status", v != null ? `status:${v}` : null)),
      facet("Route", routes.filter((y) => y[0] != null).slice(0, 6), one("route"), (v) => setKey("route", v ? `route:${v}` : null)));
  }

  // ------------------------------------------------ the lines
  const keyOf = (rw) => `${rw.node}/${rw.id}`;
  function lineEl(rw) {
    const ms = msOf(rw.ts), fields = parseJson(rw.fields);
    const fl = fields && typeof fields === "object" ? Object.entries(fields).slice(0, 5) : [];
    const lv = String(rw.level || "").toLowerCase();
    const el = h("div", { class: "al-l", role: "row", tabindex: "-1", dataset: { key: keyOf(rw) }, "aria-selected": "false" },
      h("span", { class: "al-l-t", title: whenTitle(ms) }, clockEl(ms)),
      lvEl(lv),
      h("span", { class: "al-l-s", title: rw.node + (urlOf(rw.node) ? ` · ${urlOf(rw.node)}` : "") }, nameOf(rw.node)),
      h("span", { class: "al-l-m" }, h("span", { class: "al-l-msg" }, nameUrls(rw.message)), fl.map(([k, v]) => h("span", { class: "al-l-f" }, ` ${k}=`, h("span", {}, nameUrls(plain(typeof v === "string" ? v : JSON.stringify(v)))))),
        rw.route ? h("span", { class: "al-l-f" }, ` ${rw.method || ""} ${rw.route}`) : null));
    el.onclick = () => select(rw, el);
    return el;
  }
  function select(rw, el, focus = true) {
    for (const x of list.querySelectorAll(".al-l[aria-selected=true]")) { x.setAttribute("aria-selected", "false"); x.tabIndex = -1; }
    sel = rw;
    if (el) { el.setAttribute("aria-selected", "true"); el.tabIndex = 0; if (focus) el.focus({ preventScroll: false }); }
    drawDetail(rw);
  }
  list.addEventListener("keydown", (e) => {
    const cur = e.target.closest(".al-l");
    if (!cur) return;
    const all = [...list.querySelectorAll(".al-l")];
    const i = all.indexOf(cur);
    const go = (j) => { const el = all[Math.max(0, Math.min(all.length - 1, j))]; if (el) { const rw = rows.find((x) => keyOf(x) === el.dataset.key); if (rw) select(rw, el); el.scrollIntoView({ block: "nearest" }); } };
    if (e.key === "ArrowDown" || e.key === "j") { e.preventDefault(); go(i + 1); }
    else if (e.key === "ArrowUp" || e.key === "k") { e.preventDefault(); go(i - 1); }
    else if (e.key === "Home") { e.preventDefault(); go(0); }
    else if (e.key === "End") { e.preventDefault(); go(all.length - 1); }
    else if (e.key === "Enter" || e.key === " ") { e.preventDefault(); go(i); }
    else if (e.key === "Escape" && sel) { e.preventDefault(); closeDetail(); }
  });
  function closeDetail() { sel = null; detail.classList.add("hidden"); main.classList.remove("al-wd"); for (const x of list.querySelectorAll(".al-l[aria-selected=true]")) x.setAttribute("aria-selected", "false"); }
  function firstFocusable() { const el = list.querySelector(".al-l[aria-selected=true]") || list.querySelector(".al-l"); if (el) el.tabIndex = 0; }

  function drawDetail(rw) {
    const ms = msOf(rw.ts), fields = parseJson(rw.fields) || {};
    const url = urlOf(rw.node), cp = url ? findCopy(url) : null;
    const lv = String(rw.level || "").toLowerCase();
    // The copy a line names in its error ("shard 17 (http://…)"), as a place to go.
    const named = [...String(`${rw.message} ${typeof fields === "object" ? JSON.stringify(fields) : fields}`).matchAll(URL_RE)].map((m) => m[0].replace(/[:;.\\]+$/, "")).filter((u) => findCopy(u)).filter((u, i, a) => a.indexOf(u) === i).slice(0, 2);
    const kv = [];
    const add = (k, v, tok) => {
      if (v == null || v === "") return;
      const sv = plain(typeof v === "string" ? v : JSON.stringify(v));
      kv.push(h("tr", {}, h("td", { class: "muted" }, k), h("td", { class: "mono al-dv" }, nameUrls(sv)),
        h("td", { class: "nowrap" }, h("button", { class: "btn xs ghost", title: "Only lines like this", onclick: () => addTok(tok || quote(sv.slice(0, 80))) }, "+"), h("button", { class: "btn xs ghost", title: "Leave lines like this out", onclick: () => addTok("-" + (tok || quote(sv.slice(0, 80)))) }, "−"))));
    };
    add("server", nameOf(rw.node), `server:${nameOf(rw.node)}`);
    if (typeof fields === "object") for (const [k, v] of Object.entries(fields)) add(k, v, k === "status" ? `status:${v}` : null);
    add("module", rw.target, `module:${rw.target}`);
    add("route", rw.route ? `${rw.method || ""} ${rw.route}`.trim() : null, rw.route ? `route:${rw.route}` : null);
    if (rw.status != null && !(typeof fields === "object" && fields.status != null)) add("status", rw.status, `status:${rw.status}`);
    if (rw.ms != null) add("took", fmt.ms(rw.ms));
    add("request", rw.request_id, `request:${rw.request_id}`);
    const around = (only) => link("logs", { q: only ? `server:${nameOf(rw.node)}` : "", from: Math.round(ms - 30e3), to: Math.round(ms + 30e3), at: keyOf(rw) });
    const go = [];
    const goto = (href, ic, text, onclick) => go.push(h("a", { class: "al-goto", href: href || "#", onclick }, icon(ic), h("span", { class: "grow" }, text), icon("right")));
    if (rw.request_id) {
      goto(null, "route", "This request on every server", (e) => { e.preventDefault(); setQ(`request:${rw.request_id}`); });
      goto(link("queries", { tab: "history", request: rw.request_id }), "queries", "Its queries in Queries > History");
    }
    if (fields.fingerprint) goto(link("statement/" + encodeURIComponent(fields.fingerprint)), "sql", "The statement's page");
    for (const u of named) { const c = findCopy(u); goto(link("cluster", { shard: c.shard, copy: copyName(u) }), "box", `${copyName(u)} in Cluster (named in the error)`); }
    if (cp) goto(link("cluster", { shard: cp.shard, copy: copyName(url) }), "box", `${copyName(url)} in Cluster`);
    goto(around(true), "clock", `${nameOf(rw.node)}'s lines ±30 s`);
    if (ctx.coord) goto(around(false), "clock", "Every server's lines ±30 s");
    goto(link("profile", { tab: "cpu", node: url || "" }), "flame", `Profile ${url ? nameOf(rw.node) : "this server"} now`);
    const fired = (state.alerts?.events || []).find((e) => (e.state === "firing" || e.kind === "alert_fired") && Math.abs((e.ts_ms || 0) - ms) < 10 * 60e3);
    if (fired) goto(link("alert/" + encodeURIComponent(fired.rule || fired.where?.rule)), "alerts", `Alert: ${fired.name || fired.rule} (${when(fired.ts_ms, { sec: false })})`);
    // How common the line is, in the lines at hand.
    const pat = pattern(rw.message, rw.fields);
    const same = (sample || rows).filter((x) => pattern(x.message, x.fields) === pat);
    const servers = new Set(same.map((x) => nameOf(x.node)));
    clear(detail,
      h("div", { class: "hd" }, h("span", { class: "al-l-lv al-lv-" + lv }, lv), h("b", { class: "mono" }, nameOf(rw.node)), h("span", { class: "small muted mono", title: whenTitle(ms) }, clock(ms)),
        h("span", { class: "sep" }),
        h("button", { class: "btn xs ghost", title: "Copy the line as JSON", onclick: () => copy(JSON.stringify({ ...rw, fields }), "Line copied as JSON") }, icon("copy")),
        h("button", { class: "btn xs ghost", title: "Close (Esc)", onclick: closeDetail }, icon("x"))),
      h("div", { class: "bd" },
        h("div", { class: "al-dmsg" }, nameUrls(rw.message)),
        h("table", { class: "al-dkv" }, h("tbody", {}, kv)),
        h("h4", { class: "al-dh" }, "Go to"),
        h("div", { class: "al-gotos" }, go),
        same.length > 1 ? h("div", { class: "notice small", style: { marginTop: "10px" } }, `Same pattern: ${fmt.plural(same.length, "line")} of the ${fmt.n((sample || rows).length)} at hand, on ${fmt.plural(servers.size, "server")} (${[...servers].slice(0, 3).join(", ")}${servers.size > 3 ? " …" : ""}).`) : null));
    detail.classList.remove("hidden");
    main.classList.add("al-wd");
  }

  function drawMissed(nodes) {
    ctx.missed = (nodes || []).filter((n) => n.error);
    const total = (nodes || []).length;
    if (!ctx.missed.length) { clear(missedLine); return; }
    const names = ctx.missed.map((n) => n.url ? copyName(n.url) : n.node || "?");
    const det = h("div", { class: "hidden al-missed-det" }, ctx.missed.map((n) => h("div", { class: "mono small" }, `${n.url ? copyName(n.url) : n.node}${n.shard != null ? ` (shard ${n.shard})` : ""}: ${n.error}`)));
    // L1: a server's lines are kept on it alone, so they come back with it; what the others logged about it is here now.
    const about = ctx.missed.filter((n) => n.url).slice(0, 2).map((n) => {
      const hostPort = n.url.replace(/^\w+:\/\//, "").replace(/\/.*$/, "");
      return h("button", { class: "btn xs", title: `Search every other server's lines for ${hostPort}`, onclick: () => setQ(`"${hostPort}"`) }, `What the others logged about ${copyName(n.url)}`);
    });
    clear(missedLine, h("div", { class: "al-missed" }, icon("warn"),
      h("span", {}, h("b", {}, `${ctx.missed.length} of ${total} servers did not answer`), `: ${names.slice(0, 4).join(", ")}${names.length > 4 ? ` +${names.length - 4}` : ""}. Each server keeps its own lines: theirs are missing here until they answer again.`),
      ...about,
      h("button", { class: "btn xs ghost", onclick: (e) => { det.classList.toggle("hidden"); e.currentTarget.textContent = det.classList.contains("hidden") ? "details" : "hide"; } }, "details")), det);
  }

  // ------------------------------------------------ loading
  async function refresh() {
    const my = ++gen;
    drawRange();
    writeUrl();
    if (ctx.has === false) return;
    clear(tabs, [["lines", "Lines"], ["patterns", "Patterns"]].map(([k, l]) => h("button", { role: "tab", "aria-selected": String(k === tab), onclick: () => { tab = k; writeUrl(); refresh(); } }, l)));
    clear(list, h("div", { class: "empty" }, h("div", { class: "spin" }), "Searching every server…"));
    clear(more);
    const t = timeRange(), unit = t.to - t.from > 1440 * 60e3 ? 60 : 1, col = unit === 60 ? "hour" : "minute";
    try {
      // A refresh asks every server: the histogram (every level, which the Level facet counts
      // too), the facets, and the lines. L8: without a server filter the servers, routes and
      // statuses are one query, and the facets of the same search are kept 30 s.
      const [hs, facetsJ, main0] = await Promise.all([
        ask(histSql(col)),
        facetsOf(),
        tab === "patterns" ? askPatterns() : ask(rowsSql()),
        loadEvents(),
      ]);
      if (my !== gen) return;
      const [nodes, rs] = facetsJ;
      drawMissed(main0.nodes);
      matched = drawHist(hs.rows || [], unit);
      const by = (i) => { const m = new Map(); for (const r0 of rs.rows || []) if (r0[i] != null) m.set(r0[i], (m.get(r0[i]) || 0) + r0[2]); return [...m].sort((a, b) => b[1] - a[1]); };
      drawFacets(Object.entries(levelTotals), nodes.rows || [], by(1), by(0));
      if (tab === "patterns") { buckets = main0.buckets; sample = buckets.flatMap((b) => b.rows); drawPatterns(); return; }
      rows = objects(main0); sample = null;
      [newest, oldest] = rows.length ? [msOf(rows[0].ts), msOf(rows[rows.length - 1].ts)] : [null, null];
      clear(list, rows.length ? [linesHead(), ...rows.map(lineEl)] : empty("logs", "No lines match", q ? "Try fewer filters or a longer range." : "Nothing was logged in this range at the levels the servers keep. Requests log at debug: raise a server's level in Log levels."));
      drawMore();
      countNote.textContent = `newest first · ${fmt.n(rows.length)} of ${fmt.plural(matched, "line")} · ${fmt.ms(main0.took_ms)} from ${fmt.plural((main0.nodes || []).length - ctx.missed.length || 1, "server")}`;
      const at = new URLSearchParams(location.hash.split("?")[1] || "").get("at");
      if (at) { const el = list.querySelector(`.al-l[data-key="${CSS.escape(at)}"]`); const rw = rows.find((x) => keyOf(x) === at); if (el && rw) { el.classList.add("al-hl"); select(rw, el, false); el.scrollIntoView({ block: "center" }); } }
      else if (sel) { const el = list.querySelector(`.al-l[data-key="${CSS.escape(keyOf(sel))}"]`); if (el) { el.setAttribute("aria-selected", "true"); el.tabIndex = 0; } else closeDetail(); }
      firstFocusable();
    } catch (e) {
      if (my !== gen) return;
      if (e instanceof ApiError && /no log store|log store/i.test(e.message) && ctx.has !== true) { ctx.has = false; memoryTail(); return; }
      clear(list, e instanceof ApiError && e.status === 403 ? empty("lock", "Log lines need an admin key", e.message) : h("div", { class: "error", style: { margin: "12px" } }, e.message));
      clear(hist); clear(facets); countNote.textContent = "";
    }
  }
  let facetsKey = "", facetsAt = 0, facetsHeld = null;
  /** The servers' counts and the routes' by status: [{rows: [[node, n]]}, {rows: [[route, status, n]]}]. */
  async function facetsOf() {
    const all = where(), others = where("node");
    // A live range's window moves: the same search is the same for 30 s.
    const key = (ctx.coord ? others : "") + "|" + all.replace(/ts >= '[^']*'/, "") + "|" + Math.round(timeRange().from / 30e3);
    if (key === facetsKey && Date.now() - facetsAt < 30e3 && facetsHeld) return facetsHeld;
    let out;
    if (ctx.coord && others === all) {
      const j = await ask(`SELECT node, route, status, COUNT(*) FROM logs WHERE ${all} GROUP BY node, route, status LIMIT 20000`);
      const byNode = new Map(), byRs = new Map();
      for (const [node, route, status, n] of j.rows || []) {
        byNode.set(node, (byNode.get(node) || 0) + n);
        const k = JSON.stringify([route, status]);
        byRs.set(k, (byRs.get(k) || 0) + n);
      }
      out = [{ rows: [...byNode].sort((a, b) => b[1] - a[1]) }, { rows: [...byRs].map(([k, n]) => [...JSON.parse(k), n]).sort((a, b) => b[2] - a[2]).slice(0, 300) }];
    } else {
      out = await Promise.all([
        ctx.coord ? ask(`SELECT node, COUNT(*) FROM logs WHERE ${others} GROUP BY node ORDER BY 2 DESC LIMIT 1000`) : { rows: [] },
        ask(`SELECT route, status, COUNT(*) FROM logs WHERE ${all} GROUP BY route, status ORDER BY 3 DESC LIMIT 300`)]);
    }
    [facetsKey, facetsAt, facetsHeld] = [key, Date.now(), out];
    return out;
  }
  const linesHead = () => h("div", { class: "al-l al-lh", role: "row", "aria-hidden": "true" }, h("span", { title: `Times are in ${tz}` }, `time ${tzShort}`), h("span", {}, "l", h("span", { class: "al-lv-rest" }, "evel")), h("span", {}, "server"), h("span", {}, "message"));
  function drawMore() {
    clear(more, rows.length && rows.length < matched ? [h("span", { class: "small muted" }, `${fmt.n(rows.length)} of ${fmt.plural(matched, "line")} shown`), h("button", { class: "btn sm", onclick: older }, icon("down"), "Older lines")] : null);
  }
  async function older() {
    if (oldest == null) return;
    const my = gen;
    try {
      const j = await ask(rowsSql(` AND ts <= ${lit(iso(oldest))}`));
      if (my !== gen) return;
      const seen = new Set(rows.map(keyOf));
      const add = objects(j).filter((x) => !seen.has(keyOf(x)));
      rows = rows.concat(add);
      if (add.length) oldest = msOf(add[add.length - 1].ts); else matched = rows.length;
      list.append(...add.map(lineEl));
      drawMore();
    } catch (e) { toast(e.message, "bad"); }
  }
  // Live: only the newest lines each tick (one query, not the facets), every 3 s, or every
  // 10 s over more than 20 servers (each tick asks every server); the histogram once a minute.
  let ticks = 0, lastTick = 0, lastHist = Date.now();
  async function tick() {
    const t = timeRange();
    if (!live || !t.live || tab !== "lines" || ctx.has === false || document.hidden) return;
    if (Date.now() - lastTick < (ctx.nodes.size > 20 ? 10e3 : 3e3) - 200) return;
    lastTick = Date.now();
    ticks++;
    const my = gen;
    try {
      if (newest == null) { if (ticks % 3 === 0) refresh(); return; }
      const j = await ask(rowsSql(` AND ts >= ${lit(iso(newest))}`));
      if (my !== gen) return;
      const seen = new Set(rows.slice(0, 400).map(keyOf));
      const add = objects(j).filter((x) => !seen.has(keyOf(x)));
      if (add.length >= PAGE) { refresh(); return; }
      if (add.length) {
        newest = msOf(add[0].ts);
        rows = add.concat(rows).slice(0, 2000);
        const keep = list.scrollTop;
        const head = list.querySelector(".al-lh");
        const els = add.map(lineEl);
        if (head) head.after(...els); else list.prepend(linesHead(), ...els);
        while (list.querySelectorAll(".al-l:not(.al-lh)").length > 2000) list.lastElementChild?.remove();
        if (keep > 0) list.scrollTop = keep + els.reduce((a, el) => a + el.offsetHeight, 0);
        for (const el of els) { el.classList.add("al-new"); setTimeout(() => el.classList.remove("al-new"), 1600); }
        matched += add.length;
        countNote.textContent = `newest first · live · ${fmt.plural(matched, "line")}`;
      }
      if (Date.now() - lastHist > 60e3) {
        lastHist = Date.now();
        const unit = t.to - t.from > 1440 * 60e3 ? 60 : 1, col = unit === 60 ? "hour" : "minute";
        const hs = await ask(histSql(col));
        if (my === gen) matched = drawHist(hs.rows || [], unit);
      }
    } catch { /* the next refresh says what failed */ }
  }

  /** L6: the newest lines of each group of levels the search allows, one query a group: {nodes, took_ms, buckets: [{levels, name, rows}]}. */
  async function askPatterns() {
    const lv = f().level, allowed = lv ? (lv.ge ? LV.slice(0, Math.max(0, LV.indexOf(lv.v)) + 1) : [lv.v]) : LV;
    const bs = BUCKETS.map((b) => ({ ...b, levels: b.levels.filter((l) => allowed.includes(l)) })).filter((b) => b.levels.length);
    const js = await Promise.all(bs.map((b) => ask(`SELECT ts, node, level, message, fields FROM logs WHERE ${where()} AND level IN (${b.levels.map(lit).join(", ")}) ORDER BY ts DESC, id DESC LIMIT ${b.n}`)));
    return { nodes: js[0]?.nodes, took_ms: Math.max(0, ...js.map((j) => j.took_ms || 0)), buckets: bs.map((b, i) => ({ ...b, rows: objects(js[i]) })) };
  }
  function drawPatterns() {
    const groups = new Map();
    let estimated = 0;
    for (const b of buckets) {
      // Each line of a group's sample stands for its share of that group's lines in the range.
      const total = b.levels.reduce((a, l) => a + (levelTotals[l] || 0), 0);
      const w = b.rows.length && total > b.rows.length ? total / b.rows.length : 1;
      if (w > 1) estimated++;
      for (const rw of b.rows) {
        const p = pattern(rw.message, rw.fields), key = rw.level + " " + p;
        const g = groups.get(key) || { level: String(rw.level).toLowerCase(), p, n: 0, est: false, nodes: new Set(), first: Infinity, last: 0, ex: rw };
        g.n += w; g.est ||= w > 1; g.nodes.add(nameOf(rw.node)); const ms = msOf(rw.ts); g.first = Math.min(g.first, ms); g.last = Math.max(g.last, ms);
        groups.set(key, g);
      }
    }
    const list0 = [...groups.values()].sort((a, b) => b.n - a.n);
    const max = Math.max(1, ...list0.map((g) => g.n));
    countNote.textContent = `${fmt.plural(list0.length, "pattern")} in ${fmt.plural(matched, "line")}${estimated ? ` · from the newest ${fmt.n(sample.length)}, sampled by level` : ""}`;
    clear(list, list0.length ? table([
      { key: "n", label: "lines", num: true, cls: "al-c-n", render: (g) => h("div", { class: "row nw", style: { gap: "8px", justifyContent: "flex-end" }, title: g.est ? "Estimated: its level's lines in the range, shared out as in the newest of them" : "Counted" }, h("div", { class: "barbox", style: { width: "56px" } }, h("div", { class: "bar", style: { width: g.n / max * 100 + "%", background: LCOLOR[g.level] } })), h("b", {}, (g.est ? "≈" : "") + fmt.n(Math.round(g.n)))) },
      { key: "level", label: "level", cls: "al-c-lv", render: (g) => h("span", { class: "al-l-lv al-lv-" + g.level }, g.level) },
      { key: "p", label: "message pattern", cls: "al-c-p", render: (g) => h("span", { class: "mono small al-pat2", title: g.p }, nameUrls(g.p)) },
      { key: "nodes", label: "where", cls: "al-c-where", sort: (g) => g.nodes.size, render: (g) => h("span", { class: "small nowrap", title: [...g.nodes].join(", ") }, [...g.nodes].sort().slice(0, 3).join(", ") + (g.nodes.size > 3 ? ` +${g.nodes.size - 3}` : "")) },
      { key: "last", label: "last", cls: "al-c-last", render: (g) => h("span", { class: "small nowrap", title: `first ${when(g.first)}` }, when(g.last)) },
    ], { rows: list0, compact: true, onRow: (g) => { const lit0 = g.p.split(/\s*<\w+>/)[0].trim(); tab = "lines"; setQ(`${tokens(q).filter((t) => !/^-?"/.test(t)).join(" ")} level:${g.level}${lit0 ? ` "${lit0.slice(0, 80).replace(/"/g, "")}"` : ""}`); } }) : empty("logs", "No lines match", "Try fewer filters or a longer range."));
    list.firstElementChild?.classList.add("al-ptbl");
    const short = buckets.filter((b) => b.rows.length >= b.n).map((b) => b.name);
    clear(more, h("span", { class: "small muted" }, short.length
      ? `Errors and warnings, info and debug are sampled apart (the newest ${BUCKETS.map((b) => fmt.n(b.n)).join(", ")} lines), so a server on debug does not crowd out the errors. A count with ≈ shares out its level's lines in the range as in that sample (${short.join(", ")}). Click a pattern for its lines.`
      : "Every line of the range is counted. Click a pattern for its lines."));
  }

  // ------------------------------------------------ no store: this server's memory
  let memStop = null;
  function memoryTail() {
    clear(sub, "This server keeps no log store (--log-store off): these are the latest lines in its memory, followed live. Other servers' lines are not here.");
    clear(tabs, h("button", { role: "tab", "aria-selected": "true" }, "Lines in memory"));
    clear(hist); clear(facets); clear(markLegend);
    let lastSeq = 0, mem = [];
    const x0 = () => parse(q);
    async function poll() {
      const x = x0();
      const p = new URLSearchParams({ after: String(lastSeq), limit: "1000", level: x.level?.v || "debug" });
      if (x.text.length) p.set("q", x.text.join(" ").replace(/"/g, ""));
      if (x.request_id?.[0]) p.set("request", x.request_id[0].v);
      try {
        const j = await api("/v1/logs?" + p);
        const add = (j.lines || []).map((l) => ({ ts: new Date(l.ts_ms).toISOString(), node: "this server", level: String(l.level).toLowerCase(), target: l.target, method: l.method, route: l.route, message: l.message, fields: l.fields, request_id: l.request_id, id: l.seq }));
        if (add.length) { lastSeq = j.lines[j.lines.length - 1].seq; mem = add.reverse().concat(mem).slice(0, 2000); rows = mem; clear(list, linesHead(), ...mem.slice(0, 500).map(lineEl)); firstFocusable(); }
        else if (!mem.length) clear(list, empty("logs", "No lines in memory match", "Requests log at debug."));
        countNote.textContent = `newest first · ${fmt.plural(mem.length, "line")} in memory`;
      } catch (e) { clear(list, h("div", { class: "error" }, e.message)); }
    }
    memStop?.();
    memStop = every(2000, () => (live ? poll() : null));
  }

  // ------------------------------------------------ the header
  async function loadStore() {
    try {
      const j = await api("/v1/logs/store");
      ctx.store = j;
      ctx.coord = !!j.nodes || isCoordinator();
      ctx.nodes = new Map();
      if (j.store?.node) ctx.nodes.set(j.store.node, { url: null, shard: null, self: true });
      for (const n of j.nodes || []) { const name = n.store?.node || n.node || (n.url ? copyName(n.url) : null); if (name) ctx.nodes.set(name, { url: n.url, shard: n.shard, down: !!n.error }); }
      const kept = [j.store, ...(j.nodes || []).map((n) => n.store)].filter(Boolean);
      ctx.has = kept.length > 0;
      const servers = (j.nodes ? j.nodes.length : 0) + (j.store !== undefined ? 1 : 0);
      const bytes = kept.reduce((a, x) => a + (x.bytes || 0), 0), ret = kept[0]?.retain_s;
      const down = (j.nodes || []).filter((n) => n.error).length;
      const shards = new Set((j.nodes || []).map((n) => n.shard).filter((x) => x != null)).size;
      clear(sub, ctx.has ? `Every server's lines, merged: ${fmt.plural(servers, "server")}${shards ? ` (${fmt.plural(shards, "shard")} and this coordinator)` : ""}. Newest first; click a line for where to go next.` : "");
      clear(storeBtn, icon("db"), `stores ${fmt.n(kept.length)} of ${fmt.n(servers)}${down ? ` · ${down} down` : " ok"}${ret ? ` · ${ret % 86400 === 0 ? fmt.plural(ret / 86400, "day") : fmt.dur(ret)}` : ""} · ${fmt.bytes(bytes)}`);
      storeBtn.classList.toggle("al-warnbtn", !!down);
      storeBtn.onclick = () => storesDrawer(j);
    } catch (e) {
      if (e instanceof ApiError && (e.status === 401 || e.status === 403)) { clear(list, empty("lock", "Log lines need an admin key", e.message)); ctx.has = null; return false; }
      ctx.has = false;
    }
    return true;
  }
  levelBtn.onclick = () => levelsDrawer(ctx);
  shareBtn.onclick = () => copy(location.origin + location.pathname + link("logs", { q, tab: tab === "lines" ? null : tab, ...rangeParams({ absolute: true }) }), "Link copied: it opens these lines in this window of time");

  reroute = (params) => {
    const nq = params.get("q") ?? legacyQ(params);
    const nt = params.get("tab") === "patterns" ? "patterns" : "lines";
    alertId = params.get("alert") || null;
    if (nq !== q || nt !== tab) { q = nq; tab = nt; drawChips(); }
    refresh();
  };
  q = r.params.get("q") ?? legacyQ(r.params);
  tab = r.params.get("tab") === "patterns" ? "patterns" : "lines";
  if (r.params.get("tab") === "live") live = true;
  drawChips(); drawRange();
  loadStore().then((ok) => { if (!ok) return; if (ctx.has === false) memoryTail(); else refresh(); });
  const offs = [bus.on("range", () => { if (ctx.has) refresh(); else drawRange(); }), bus.on("paused", drawRange), bus.on("cluster", () => { if (!state.paused && sel) drawDetail(sel); })];
  const stop = every(1000, tick, false);
  return () => { stop(); memStop?.(); offs.forEach((x) => x()); reroute = null; };
}
/** The old Logs links (?request=, ?node=, ?shard=, ?level=) as a search. */
function legacyQ(p) {
  return [p.get("level") ? `level>=${p.get("level")}` : null, p.get("node") ? `server:${quote(p.get("node"))}` : null, p.get("shard") ? `shard:${p.get("shard")}` : null, p.get("request") ? `request:${p.get("request")}` : null].filter(Boolean).join(" ");
}

// ---------------------------------------------------------------- stores
function storesDrawer(j) {
  const rows = [j.store ? { url: null, shard: null, store: j.store, self: true } : null, ...(j.nodes || [])].filter(Boolean);
  const period = (sec) => sec % 86400 === 0 ? fmt.plural(sec / 86400, "day") : sec % 3600 === 0 ? fmt.plural(sec / 3600, "hour") : fmt.dur(sec);
  drawer({
    title: "Log stores", sub: "a ChloeDB database beside each server's, with the tables logs and queries", wide: true,
    body: [
      table([
        { key: "url", label: "server", render: (x) => h("span", { class: "mono small", title: x.url || "this server" }, x.self ? `${x.store?.node || "this server"} (this one)` : x.url && findCopy(x.url) ? copyName(x.url) : x.store?.node || x.url) },
        { key: "shard", label: "shard", num: true, render: (x) => x.shard ?? "–" },
        { key: "lines", label: "lines", num: true, sort: (x) => x.store?.tables?.logs?.rows, render: (x) => x.store ? fmt.n(x.store.tables?.logs?.rows) : "–" },
        { key: "bytes", label: "size", num: true, sort: (x) => x.store?.bytes, render: (x) => x.store ? `${fmt.bytes(x.store.bytes)} of ${fmt.bytes(x.store.max_bytes)}` : "–" },
        { key: "kept", label: "kept", render: (x) => x.store?.retain_s ? period(x.store.retain_s) : "–" },
        { key: "written", label: "last write", render: (x) => x.store?.last_write_ms ? when(x.store.last_write_ms) : "–" },
        { key: "state", label: "", sort: (x) => (x.error ? 0 : 1), render: (x) => x.error ? h("span", { class: "pill bad", title: x.error }, icon("bad"), "did not answer") : !x.store ? pill("disabled", "no store") : x.store.last_error ? h("span", { class: "pill bad", title: x.store.last_error }, icon("bad"), "cannot write") : (x.store.dropped?.lines || 0) ? pill("warn", `${fmt.n(x.store.dropped.lines)} dropped`) : pill("ok", "ok") },
      ], { rows, compact: true, sortKey: "state", desc: false }),
      h("p", { class: "small muted" }, "Each server writes its lines and queries to its own store every second and deletes what is older than its retention or past its size; a search on a coordinator asks every server and merges the answers. A server that does not answer is missing from the results, and said so. Flags: --log-store, --log-retain, --log-store-mb, --log-queries, --node-name."),
    ],
  });
}

// ---------------------------------------------------------------- log levels
// Trace is not a preset (L5): it can still be typed in the field.
const PRESETS = [["error", "error"], ["warn", "warn"], ["info", "info"], ["debug", "debug"], ["info,chloedb_server=debug", "server debug"]];
const SET = {};
const EXPIRY = [[900, "15 min"], [1800, "30 min"], [3600, "1 h"], [14400, "4 h"]];
async function levelsDrawer(ctx) {
  const body = h("div", { class: "stack" });
  const d = drawer({ title: "Log levels", sub: "what each server logs; a change reverts by itself when it expires", wide: true, body });
  let self = null;
  try { self = await api("/v1/logs?limit=1&level=error"); } catch { /* shown below */ }
  const me = ctx.store?.store?.node;
  const servers = [{ name: me ? `${me} (this server)` : "this server", node: me, url: null, filter: self?.filter, expires_ms: self?.filter_expires_ms ?? self?.expires_ms, set_by: self?.filter_set_by, default: self?.default_filter }];
  // Another server's level: what it answers (GET /v1/logs?node=, below); until then, the ones changed from this page.
  for (const [node, n] of ctx.nodes) if (!n.self && n.url) { const k = SET[n.url]; const live = k && (!k.expires_ms || k.expires_ms > Date.now()); servers.push({ name: findCopy(n.url) ? copyName(n.url) : node, node, url: n.url, shard: n.shard, down: n.down, filter: live ? k.filter : null, expires_ms: live ? k.expires_ms : null, set_by: live ? k.set_by : null, default: k?.previous }); }
  // A server that reports when its level expires also sets another server's (PUT ?node=).
  const perServer = !!self && ("filter_expires_ms" in self || "filter_set_by" in self || !!self.per_server);
  ctx.newer = self ? perServer : ctx.newer;
  // L5: each other server's own level, 16 at a time; the changed ones first.
  const others = servers.filter((x) => x.url && !x.down);
  const changed = (x) => !!(x.expires_ms || x.set_by);
  const find = servers.length > 12 ? h("input", { class: "input sm", placeholder: "Filter servers", style: { width: "220px" }, oninput: debounce(() => draw(), 200) }) : null;
  const tbody = h("tbody");
  const draw = () => {
    const q = (find?.value || "").trim().toLowerCase();
    const xs = servers.filter((x) => !q || x.name.toLowerCase().includes(q) || (x.node || "").toLowerCase().includes(q)).sort((a, b) => (a.url ? 1 : 0) - (b.url ? 1 : 0) || Number(changed(b)) - Number(changed(a)));
    clear(tbody, xs.map(row));
  };
  const row = (x) => h("tr", {},
    h("td", {}, h("span", { class: "mono small", title: x.url || "" }, x.name), x.shard != null ? h("span", { class: "tiny muted" }, ` shard ${x.shard}`) : null),
    h("td", { class: "mono small", title: x.err ? x.err : "" }, x.filter || (x.down ? "does not answer" : x.err ? "did not answer" : x.url && !x.read && perServer ? "reading…" : x.url ? "not reported" : "unknown")),
    h("td", { class: "small" }, x.expires_ms ? `goes back at ${when(x.expires_ms, { sec: false })}${x.default ? ` to ${x.default}` : ""}${x.set_by ? ` · set by ${x.set_by}` : ""}` : x.filter ? (perServer ? "from the command line" : "until changed back") : ""),
    h("td", {}, (() => {
      const b = h("button", { class: "btn xs", onclick: () => change(x) }, "Change…");
      if (!can("admin", "PUT /v1/logs/filter")) { b.disabled = true; b.title = needs("admin", "PUT /v1/logs/filter"); }
      else if (x.url && !perServer) { b.disabled = true; b.title = "This server sets only its own level: a newer chloedb-server sets another server's"; }
      else if (x.down) { b.disabled = true; b.title = "It does not answer"; }
      return b;
    })()));
  draw();
  clear(body,
    find ? h("div", { class: "row", style: { gap: "8px" } }, find, h("span", { class: "small muted" }, "changed levels first")) : null,
    h("div", { class: "tbl-wrap", style: { maxHeight: "60vh" } }, h("table", { class: "t compact" }, h("thead", {}, h("tr", {}, ["server", "logs", "until", ""].map((k) => h("th", {}, k)))), tbody)),
    h("p", { class: "small muted" }, "A level is error, warn, info, debug or trace, or target=level pairs as in --log (info,chloedb_server=debug). Requests log at debug: debug on a busy server writes a line a request, so it expires. Every change is audited and shows as an event."));
  if (perServer) {
    for (let i = 0; i < others.length; i += 16) {
      await Promise.all(others.slice(i, i + 16).map(async (x) => {
        try {
          const j = await api("/v1/logs?node=" + encodeURIComponent(x.url));
          Object.assign(x, { filter: j.filter ?? x.filter, expires_ms: j.filter_expires_ms ?? null, set_by: j.filter_set_by ?? null, default: j.previous ?? x.default, read: true });
        } catch (e) { x.err = e.message; }
      }));
      if (!body.isConnected) return;
      draw();
    }
  }
  async function change(x) {
    // Expiry came with the events (one server version): an older server keeps a level until changed.
    const expiring = ctx.newer !== false;
    let filter = /^(error|warn|info)$/.test(x.filter || "info") ? "debug" : "info", exp = 1800;
    const inp = h("input", { class: "input mono", value: filter, style: { width: "100%" }, "aria-label": "Log filter", oninput: () => { filter = inp.value.trim(); presets.set(filter); } });
    const presets = seg(PRESETS, filter, (v) => { filter = v; inp.value = v; });
    const expiry = seg(EXPIRY.map(([k, l]) => [k, l]), exp, (v) => { exp = v; });
    const pick = await new Promise((resolve) => {
      let done = false;
      const ok = h("button", { class: "btn primary" }, "Next");
      const no = h("button", { class: "btn" }, "Cancel");
      const m = modal({ title: `What ${x.name} logs`, body: [h("label", { class: "f" }, "Level", presets), inp,
        expiring ? h("label", { class: "f" }, "Goes back after", expiry) : h("div", { class: "notice small" }, `This server keeps the new level until someone changes it back: a newer chloedb-server sets it back by itself. Now: ${x.filter || "unknown"}.`)], foot: [no, ok], onClose: () => { if (!done) resolve(null); } });
      ok.onclick = () => { done = true; m.close(); resolve({ filter, exp }); };
      no.onclick = () => { done = true; m.close(); resolve(null); };
      setTimeout(() => no.focus(), 0);
    });
    if (!pick?.filter) return;
    const until = Date.now() + pick.exp * 1000;
    const cap = (t) => t.charAt(0).toUpperCase() + t.slice(1);
    const heavy = /debug|trace/.test(pick.filter) ? "Debug writes a line a request: on a busy server that is thousands a second, kept in its log store. " : "";
    const okc = await confirm({
      title: expiring ? `${cap(x.name)} logs ${pick.filter} until ${when(until, { sec: false })}?` : `${cap(x.name)} logs ${pick.filter} until changed back?`,
      body: heavy + (expiring ? `It goes back to ${x.default || x.filter || "its level from the command line"} by itself at ${when(until, { sec: false })}.` : `This server does not set it back by itself: change it back to ${x.filter || "its level"} when you are done.`),
      ok: "Change the level", reason: true });
    if (!okc) return;
    try {
      const j = await api("/v1/logs/filter" + (x.url ? "?node=" + encodeURIComponent(x.url) : ""), { method: "PUT", body: { filter: pick.filter, expires_s: pick.exp }, reason: okc.reason });
      if (x.url && j.node !== x.url) throw new Error(`the answer came from ${j.node || "this server"}, not ${x.name}: nothing was changed there`);
      if (x.url) SET[x.url] = { filter: j.filter, expires_ms: j.expires_ms, set_by: j.set_by, previous: j.previous };
      const exp0 = j.expires_ms || (j.expires_s ? Date.now() + j.expires_s * 1000 : null);
      toast(exp0 ? `${x.name} logs ${j.filter} until ${when(exp0, { sec: false })}` : `${x.name} logs ${j.filter} until it is changed back`, exp0 ? "ok" : "warn", 6000);
      d.close(); levelsDrawer(ctx);
    } catch (e) { toast(e instanceof ApiError && e.status === 403 ? `Changing the log level ${needs("admin")}` : e.message, "bad"); }
  }
}
