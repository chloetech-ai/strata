// Recsys eval: what the rankings served, as the rank log recorded it, set
// against what the users did next. The log is exported with each row's
// label from a table of events (the best grade of the user's events on
// the row within a window after it was served), and the measures are
// worked out here per variant (an A/B arm): nDCG, hit rate and MRR at k,
// labels by position, how many items each variant served, which of the
// ranking's values tell good rows from the rest, and whether a variant's
// lead over the control is more than chance.

import { h, s, icon, clear, fmt, api, store, toast, empty, seg, failure, drawer, authHeader, sqlBlock, copy, showTip, hideTip, ApiError } from "../core.js";
import { tables, table as tableOf } from "../catalog.js";
import { xyChart, SERIES } from "../charts.js";
import { quoteName } from "../sqledit.js";

export default { id: "recsys", title: "Recsys eval", icon: "recsys", group: "Tune", full: true, keywords: "recommender recsys evaluation rank log a/b test variants ndcg hit rate mrr ctr position bias labels events clicks purchases offline metrics training export features auc", render };

const LOG_COLS = ["request", "row_key", "position", "shown", "features", "variant", "label", "context", "ts"];
export const isLog = (t) => LOG_COLS.every((c) => (t.short?.fields || []).some((f) => f.name === c));
const WINDOWS = [["5 minutes", "5 min"], ["30 minutes", "30 min"], ["1 hour", "1 h"], ["1 day", "1 day"]];
const SINCE = [[1, "24 h"], [7, "7 days"], [30, "30 days"], [0, "all"]];
const KNOWN = { view: 0, impression: 0, open: 0, click: 1, like: 1, play: 1, read: 1, save: 2, cart: 2, add_to_cart: 2, share: 2, buy: 3, purchase: 3, order: 3, subscribe: 3 };
const MAX_RECORDS = 300000;
const LABEL_FILL = ["var(--surface-3)", "var(--seq-3)", "var(--seq-5)", "var(--seq-7)"];
const labelFill = (l) => LABEL_FILL[Math.max(0, Math.min(3, Math.round(l || 0)))];

/** The events table and its columns, guessed from the names a log's export reads. */
function guessEvents(ts, log) {
  for (const t of ts) {
    if (t.table === log || isLog(t)) continue;
    const fs = t.short?.fields || [];
    const time = fs.find((f) => f.type === "timestamp");
    if (!time) continue;
    const cols = fs.filter((f) => (f.type === "keyword" || f.type === "int") && !f.multi);
    const pick = (names) => cols.find((f) => names.includes(f.name.toLowerCase()))?.name || "";
    const user = pick(["usr", "user", "user_id", "userid", "context", "session", "customer", "visitor"]);
    const item = pick(["item", "item_id", "product", "product_id", "doc", "doc_id", "row_key", "video", "track", "article"]);
    if (user && item) return { table: t.table, user, item, time: time.name, action: pick(["action", "event", "type", "kind", "verb"]) };
  }
  return null;
}

// ---------------------------------------------------------------- measures

/** A request's measures at k: nDCG (null with no good row logged), hit, reciprocal rank, gain. */
export function requestMeasures(rows, k) {
  const shown = rows.filter((r) => r.shown).sort((a, b) => a.position - b.position).slice(0, k);
  const dcg = shown.reduce((a, r, i) => a + (2 ** (r.label || 0) - 1) / Math.log2(i + 2), 0);
  const ideal = rows.map((r) => r.label || 0).sort((a, b) => b - a).slice(0, k).reduce((a, l, i) => a + (2 ** l - 1) / Math.log2(i + 2), 0);
  const first = shown.findIndex((r) => (r.label || 0) > 0);
  return { ndcg: ideal > 0 ? dcg / ideal : null, hit: first >= 0 ? 1 : 0, rr: first >= 0 ? 1 / (first + 1) : 0, gain: shown.reduce((a, r) => a + (r.label || 0), 0), shown };
}

const mean = (xs) => xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : null;
function variance(xs, m = mean(xs)) { return xs.length > 1 ? xs.reduce((a, x) => a + (x - m) ** 2, 0) / (xs.length - 1) : 0; }
/** The standard normal's distribution function (Abramowitz and Stegun 7.1.26). */
function phi(z) {
  const t = 1 / (1 + 0.3275911 * Math.abs(z) / Math.SQRT2);
  const y = 1 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * Math.exp(-z * z / 2);
  return z >= 0 ? (1 + y) / 2 : (1 - y) / 2;
}
/** Welch's test of two samples' means: the difference, its 95% interval and the two-sided p (normal, for the hundreds of requests a test has). */
export function compare(a, b) {
  if (a.length < 2 || b.length < 2) return null;
  const ma = mean(a), mb = mean(b);
  const se = Math.sqrt(variance(a, ma) / a.length + variance(b, mb) / b.length);
  const d = mb - ma;
  const p = se > 0 ? 2 * (1 - phi(Math.abs(d / se))) : (d === 0 ? 1 : 0);
  return { ma, mb, d, lo: d - 1.96 * se, hi: d + 1.96 * se, p, lift: ma ? d / ma : null };
}
/** The area under the ROC curve of a value telling positives from the rest (0.5: no better than chance). */
export function auc(pairs) {
  const ps = pairs.filter(([v]) => v != null && Number.isFinite(v)).sort((x, y) => x[0] - y[0]);
  const npos = ps.filter(([, y]) => y).length, nneg = ps.length - npos;
  if (!npos || !nneg) return null;
  let rankSum = 0;
  for (let i = 0; i < ps.length;) {
    let j = i; while (j < ps.length && ps[j][0] === ps[i][0]) j++;
    const avg = (i + j + 1) / 2;
    for (let q = i; q < j; q++) if (ps[q][1]) rankSum += avg;
    i = j;
  }
  return (rankSum - npos * (npos + 1) / 2) / (npos * nneg);
}

/** Everything the view draws, from the exported records. */
function evaluate(recs, k) {
  const reqs = new Map();
  for (const r of recs) {
    let q = reqs.get(r.request);
    if (!q) reqs.set(r.request, q = { id: r.request, ts: r.ts, context: r.context, variant: r.variant ?? "(none)", rows: [], history: r.history_items || [], historyActions: r.history_actions || [] });
    q.rows.push(r);
  }
  const requests = [...reqs.values()];
  const names = [...new Set(requests.map((q) => q.variant))].sort();
  const V = new Map(names.map((n, i) => [n, { name: n, color: SERIES[i % SERIES.length], reqs: [], ndcg: [], hit: [], rr: [], gain: [], impressions: 0, positives: 0, items: new Map(), byPos: new Map() }]));
  let maxShown = 0, positives = 0, shownRows = 0;
  for (const q of requests) {
    const m = requestMeasures(q.rows, k);
    Object.assign(q, m);
    const v = V.get(q.variant);
    v.reqs.push(q); v.hit.push(m.hit); v.rr.push(m.rr); v.gain.push(m.gain);
    if (m.ndcg != null) v.ndcg.push(m.ndcg);
    for (const r of m.shown) { v.impressions++; shownRows++; if ((r.label || 0) > 0) { v.positives++; positives++; } v.items.set(r.row_key, (v.items.get(r.row_key) || 0) + 1); }
    for (const r of q.rows) {
      const b = v.byPos.get(r.position) || { n: 0, pos: 0, shown: 0 };
      b.n++; if ((r.label || 0) > 0) b.pos++; if (r.shown) b.shown++;
      v.byPos.set(r.position, b);
      if (r.shown) maxShown = Math.max(maxShown, r.position + 1);
    }
  }
  for (const v of V.values()) {
    const counts = [...v.items.values()].sort((a, b) => b - a);
    const top = Math.max(1, Math.ceil(counts.length / 10));
    v.coverage = v.items.size;
    v.concentration = v.impressions ? counts.slice(0, top).reduce((a, b) => a + b, 0) / v.impressions : null;
    v.m = { ndcg: mean(v.ndcg), hit: mean(v.hit), rr: mean(v.rr), gain: mean(v.gain), ctr: v.impressions ? v.positives / v.impressions : null };
  }
  // Which values tell good rows from the rest, over the top k shown.
  const featCount = new Map();
  for (const q of requests) for (const r of q.shown) for (const f of Object.keys(r.features || {})) featCount.set(f, (featCount.get(f) || 0) + 1);
  const feats = [...featCount.entries()].filter(([, n]) => n >= shownRows * 0.05).sort((a, b) => b[1] - a[1]).slice(0, 12).map(([f, n]) => {
    const pairs = requests.flatMap((q) => q.shown.map((r) => [r.features?.[f], (r.label || 0) > 0]));
    return { name: f, n, auc: auc(pairs) };
  }).filter((f) => f.auc != null).sort((a, b) => Math.abs(b.auc - 0.5) - Math.abs(a.auc - 0.5));
  const scoreAuc = names.map((n) => ({ name: n, auc: auc(V.get(n).reqs.flatMap((q) => q.shown.map((r) => [r.score, (r.label || 0) > 0]))) }));
  const t0 = Math.min(...requests.map((q) => q.ts)), t1 = Math.max(...requests.map((q) => q.ts));
  return { requests, variants: [...V.values()], maxShown, positives, shownRows, feats, scoreAuc, t0, t1, records: recs.length };
}

// ---------------------------------------------------------------- the view

function render(page) {
  let ts = [], st = store.get("recsys.state", null), out = null, actions = [], aborter = null, itemTitles = new Map(), segments = null;
  const save = () => store.set("recsys.state", st);
  const topBar = h("div", { class: "rx-top" });
  const flow = h("div", { class: "rx-flow" });
  const body = h("div", { class: "rx-body" });
  page.append(h("div", { class: "rx" }, topBar, flow, body));

  tables().then(async (list) => {
    ts = list;
    const logs = ts.filter(isLog);
    if (!logs.length) { drawNoLog(); return; }
    if (!st || !tableOf(st.log)) st = { log: logs[0].table, events: guessEvents(ts, logs[0].table), window: "1 hour", since: 7, k: 10, grades: null, control: null, metric: "ndcg", only: "", segment: "" };
    await loadActions();
    drawTop(); run();
  }).catch((e) => clear(body, failure(e, "The tables")));

  function drawNoLog() {
    clear(topBar);
    clear(body, h("div", { class: "card rx-none" },
      empty("recsys", "No rank log yet", "A query run with a log writes what it served, row by row, with the values its ranking read. Log a ranked query and come back."),
      h("div", { class: "rx-howto" },
        h("div", { class: "small muted" }, "Over HTTP: add \"log\" to a query, with whom it served and the ranking's A/B arm"),
        sqlBlock(`POST /v1/query\n{"sql": "SELECT id, title FROM docs ORDER BY bm25(body, :q) DESC LIMIT 10",\n "params": {"q": "quiet charger"},\n "log": {"context": "u42", "variant": "b", "candidates": 20}}`),
        h("div", { class: "small muted" }, "Then keep what the users did in a table of events (user, item, time, action), and this view sets one against the other."))));
  }

  async function loadActions() {
    actions = [];
    const e = st.events;
    if (e?.action && tableOf(e.table)) {
      try {
        const j = await api("/v1/sql", { body: { sql: `SELECT ${quoteName(e.action)} AS a, COUNT(*) AS n FROM ${quoteName(e.table)} GROUP BY ${quoteName(e.action)} ORDER BY n DESC LIMIT 16` } });
        actions = j.rows.map(([a, n]) => ({ name: String(a), n }));
      } catch { actions = []; }
    }
    if (!st.grades || !actions.every((a) => a.name in st.grades)) {
      const g = {};
      for (const a of actions) g[a.name] = st.grades?.[a.name] ?? KNOWN[a.name.toLowerCase()] ?? 1;
      st.grades = g; save();
    }
  }

  // ------------------------------------------------------------ the controls
  function drawTop() {
    const logs = ts.filter(isLog);
    const ev = st.events || {};
    const et = tableOf(ev.table);
    const fsel = (key, types, allowNone) => h("select", { class: "select sm mono", title: key, onchange: async (e) => { st.events = { ...ev, [key]: e.target.value }; save(); if (key === "action") await loadActions(); drawTop(); run(); } },
      allowNone ? h("option", { value: "" }, "(none)") : null,
      (et?.short?.fields || []).filter((f) => types.includes(f.type) && !f.multi).map((f) => h("option", { value: f.name, selected: f.name === ev[key] }, f.name)));
    const grade = (a) => {
      const v = st.grades[a.name] ?? 0;
      const set = (x) => { st.grades[a.name] = Math.max(0, Math.min(3, x)); save(); drawTop(); run(); };
      return h("span", { class: "rx-grade", title: `${fmt.n(a.n)} events; grade ${v}: ${["not a label", "fair", "good", "best"][v]}` },
        h("span", { class: "rx-gdot", style: { background: labelFill(v) } }), h("span", { class: "mono" }, a.name),
        h("button", { class: "rx-gb", "aria-label": "lower", onclick: () => set(v - 1) }, "−"), h("b", {}, v), h("button", { class: "rx-gb", "aria-label": "higher", onclick: () => set(v + 1) }, "+"));
    };
    clear(topBar,
      h("div", { class: "rx-ctl" },
        h("label", { class: "rx-lbl" }, "log", h("select", { class: "select sm", onchange: (e) => { st.log = e.target.value; save(); run(); } }, logs.map((t) => h("option", { value: t.table, selected: t.table === st.log }, t.table)))),
        h("label", { class: "rx-lbl" }, "events", h("select", { class: "select sm", onchange: async (e) => { const t = tableOf(e.target.value); st.events = t ? guessEvents([t], st.log) || { table: t.table, user: "", item: "", time: "", action: "" } : null; save(); await loadActions(); drawTop(); run(); } },
          h("option", { value: "" }, "(none: labels written in the log)"), ts.filter((t) => !isLog(t)).map((t) => h("option", { value: t.table, selected: t.table === ev.table }, t.table)))),
        et ? h("span", { class: "rx-map", title: "Which columns of the events are the user, the item, the time and the action" },
          h("span", { class: "muted" }, "user"), fsel("user", ["keyword", "int"]), h("span", { class: "muted" }, "item"), fsel("item", ["keyword", "int"]),
          h("span", { class: "muted" }, "time"), fsel("time", ["timestamp", "int"]), h("span", { class: "muted" }, "action"), fsel("action", ["keyword"], true)) : null),
      h("div", { class: "rx-ctl" },
        h("label", { class: "rx-lbl" }, "label window", seg(WINDOWS, st.window, (v) => { st.window = v; save(); run(); })),
        h("label", { class: "rx-lbl" }, "served in the last", seg(SINCE, st.since, (v) => { st.since = v; save(); run(); })),
        h("label", { class: "rx-lbl" }, "at k", seg([[5, "5"], [10, "10"], [20, "20"]], st.k, (v) => { st.k = v; save(); if (out) { out = { ...evaluate(out.recs, v), recs: out.recs }; draw(); } })),
        h("span", { class: "sep" }),
        h("button", { class: "btn sm", title: "Download the records for training, as NDJSON", onclick: download }, icon("download"), "Training data"),
        h("button", { class: "btn sm ghost", title: "Copy the export as a curl command", onclick: () => copy(`curl -s ${location.origin}/v1/rank_log/export -H 'content-type: application/json' -d '${JSON.stringify(exportBody())}'`, "Copied the curl command") }, icon("copy"), "curl"),
        h("button", { class: "btn sm primary", onclick: run }, icon("refresh"), "Evaluate")),
      actions.length ? h("div", { class: "rx-ctl rx-grades" }, h("span", { class: "small muted", title: "The label of a row is the best grade among the user's events on it within the window after it was served" }, "grades"), actions.map(grade)) : null);
  }

  function exportBody() {
    const b = { table: st.log, history: 10 };
    const e = st.events;
    if (e?.table && e.user && e.item && e.time) {
      b.events = { table: e.table, user: e.user, item: e.item, time: e.time, ...(e.action ? { action: e.action } : {}) };
      b.window = st.window;
      if (e.action) b.grades = Object.fromEntries(Object.entries(st.grades || {}).filter(([, g]) => g > 0));
    }
    if (st.since) b.since = new Date(Date.now() - st.since * 86400e3).toISOString().slice(0, 19) + "Z";
    return b;
  }

  async function download() {
    try {
      const r = await fetch("/v1/rank_log/export", { method: "POST", headers: { "content-type": "application/json", ...authHeader() }, body: JSON.stringify(exportBody()) });
      if (!r.ok) throw new ApiError(r.status, (await r.json().catch(() => ({}))).error || "HTTP " + r.status);
      const blob = await r.blob();
      const a = h("a", { href: URL.createObjectURL(blob), download: `${st.log}.ndjson` }); document.body.append(a); a.click(); a.remove();
      setTimeout(() => URL.revokeObjectURL(a.href), 2000);
    } catch (e) { toast("The export failed: " + e.message, "bad"); }
  }

  // ------------------------------------------------------------ the export
  async function run() {
    aborter?.abort();
    const ab = aborter = new AbortController();
    const recs = [];
    const t0 = performance.now();
    drawFlow(null, "Reading the log…");
    if (!out) clear(body, h("div", { class: "rx-wait" }, h("span", { class: "spin" }), "Reading the log and the events…"));
    try {
      const r = await fetch("/v1/rank_log/export", { method: "POST", headers: { "content-type": "application/json", ...authHeader() }, body: JSON.stringify(exportBody()), signal: ab.signal });
      if (!r.ok) throw new ApiError(r.status, (await r.json().catch(() => ({}))).error || "HTTP " + r.status);
      const rd = r.body.getReader(), dec = new TextDecoder();
      let buf = "", last = 0;
      for (;;) {
        const { value, done } = await rd.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        let i;
        while ((i = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, i); buf = buf.slice(i + 1);
          if (!line.trim()) continue;
          const x = JSON.parse(line);
          if (x.error) throw new Error(x.error);
          recs.push(x);
        }
        if (recs.length >= MAX_RECORDS) { ab.abort(); break; }
        if (recs.length - last > 5000) { last = recs.length; drawFlow(null, `Read ${fmt.n(recs.length)} rows…`); }
      }
    } catch (e) {
      if (ab.signal.aborted && recs.length < MAX_RECORDS) return;
      if (!(ab.signal.aborted && recs.length >= MAX_RECORDS)) { clear(body, failure(e, "The export")); drawFlow(null, ""); return; }
    }
    if (ab !== aborter) return;
    if (!recs.length) { out = null; drawFlow(null, ""); clear(body, empty("recsys", "Nothing logged in this period", "Choose a longer period, or log some queries.")); return; }
    out = { ...evaluate(recs, st.k), recs, took: performance.now() - t0, capped: recs.length >= MAX_RECORDS };
    if (!st.control || !out.variants.some((v) => v.name === st.control)) { st.control = out.variants[0].name; save(); }
    await Promise.all([loadSegments(), loadEventCount()]);
    drawFlow(out); draw();
  }

  let eventCount = null;
  async function loadEventCount() {
    eventCount = null;
    const e = st.events;
    if (!e?.table) return;
    try { eventCount = (await api("/v1/sql", { body: { sql: `SELECT COUNT(*) FROM ${quoteName(e.table)}` } })).rows[0][0]; } catch { /* left out */ }
  }

  /** The table the events' users point at, for a breakdown by one of its fields. */
  const userTable = () => { const e = st.events; const f = tableOf(e?.table)?.short?.fields?.find((x) => x.name === e?.user); return f?.references ? tableOf(f.references) : null; };
  const itemTable = () => { const e = st.events; const f = tableOf(e?.table)?.short?.fields?.find((x) => x.name === e?.item); return f?.references ? tableOf(f.references) : null; };
  async function loadSegments() {
    segments = null;
    const ut = userTable();
    if (!ut || !st.segment || !out) return;
    const ctx = [...new Set(out.requests.map((q) => q.context).filter((c) => c != null))];
    const map = new Map();
    for (let i = 0; i < ctx.length && i < 5000; i += 500) {
      const list = ctx.slice(i, i + 500).map((c) => `'${String(c).replace(/'/g, "''")}'`).join(", ");
      try {
        const j = await api("/v1/sql", { body: { sql: `SELECT ${quoteName(ut.short.key)}, ${quoteName(st.segment)} FROM ${quoteName(ut.table)} WHERE ${quoteName(ut.short.key)} IN (${list}) LIMIT 500` } });
        for (const [k, v] of j.rows) map.set(String(k), v == null ? "(none)" : String(v));
      } catch { return; }
    }
    segments = map;
  }

  // ------------------------------------------------------------ the flow
  function drawFlow(o, status) {
    const ev = st.events;
    const node = (ic, title, lines, onclick, cls = "") => h(onclick ? "button" : "div", { class: "rx-node " + cls, onclick, title: onclick ? "Open it in the SQL workbench" : null }, h("div", { class: "rx-nh" }, icon(ic), h("b", {}, title)), lines.filter(Boolean).map((l) => h("div", { class: "small dim" }, l)));
    const arrow = (label) => h("div", { class: "rx-arrow" }, h("span", { class: "small muted" }, label || ""), s("svg", { viewBox: "0 0 60 12", preserveAspectRatio: "none" }, s("path", { d: "M0 6H54", class: "rx-al" }), s("path", { d: "M50 1.5 58 6 50 10.5", class: "rx-ah" })));
    const open = (t) => () => { location.hash = "#/sql?q=" + encodeURIComponent(`SELECT * FROM ${quoteName(t)} ORDER BY ts DESC LIMIT 100`); };
    const graded = Object.entries(st.grades || {}).filter(([, g]) => g > 0).map(([a, g]) => `${a} ${g}`).join(" · ");
    clear(flow,
      node("db", st.log, o ? [`${fmt.plural(o.records, "row")} · ${fmt.plural(o.requests.length, "request")}`, `${fmt.plural(o.variants.length, "variant")}: ${o.variants.map((v) => v.name).join(", ")}`] : [status || "…"], open(st.log)),
      arrow(ev?.table ? "joined by user and item" : ""),
      ev?.table ? node("streams", ev.table, [eventCount != null ? fmt.plural(eventCount, "event") : "", actions.length ? actions.map((a) => a.name).join(", ") : ""], open(ev.table)) : node("streams", "no events", ["labels written in the log only"]),
      arrow(ev?.table ? `within ${st.window}` : ""),
      node("check", "labels", o ? [`${fmt.n(o.positives)} good of ${fmt.n(o.shownRows)} shown (${fmt.pct(o.shownRows ? o.positives / o.shownRows : 0)})`, graded ? "grades: " + graded : "any event grades 1"] : [""]),
      arrow(""),
      node("relevance", `measures at ${st.k}`, o ? [`nDCG, hit rate, MRR per variant`, `${fmt.ms(o.took)} for the export and the sums${o.capped ? `, first ${fmt.n(MAX_RECORDS)} rows` : ""}`] : [""], null, "last"));
  }

  // ------------------------------------------------------------ the results
  function draw() {
    if (!out) return;
    clear(body,
      scoreboard(),
      h("div", { class: "rx-row" }, positionCard(), featureCard()),
      h("div", { class: "rx-row" }, timeCard(), segmentCard()),
      requestsCard());
  }

  const METRICS = [["ndcg", "nDCG", "Discounted gain of the labels in the order shown, over the best order of the request's logged rows; requests with a good row only"], ["hit", "Hit rate", "Requests with a good row in the top k"], ["rr", "MRR", "The reciprocal rank of the first good row (0 with none)"], ["gain", "Gain", "The sum of the labels in the top k, per request"]];

  function scoreboard() {
    const ctl = out.variants.find((v) => v.name === st.control) || out.variants[0];
    const cards = out.variants.map((v) => {
      const isCtl = v === ctl;
      const tests = Object.fromEntries(METRICS.map(([m]) => [m, isCtl ? null : compare(ctl[m], v[m])]));
      const tile = ([m, label, what]) => {
        const t = tests[m];
        const sig = t && t.p < 0.05;
        return h("div", { class: "rl-m", title: what },
          h("span", { class: "lbl" }, label), h("b", {}, m === "gain" ? fmt.n(v.m[m], 2) : fmt.n(v.m[m], 3)),
          t ? h("span", { class: "d " + (sig ? (t.d > 0 ? "up" : "down") : "muted"), title: `${fmt.pct(t.lift ?? 0)} vs ${ctl.name}; 95% interval of the difference ${fmt.n(t.lo, 3)} to ${fmt.n(t.hi, 3)}; p = ${fmt.n(t.p, 4)}` }, `${t.d > 0 ? "+" : ""}${fmt.pct(t.lift ?? 0)}${sig ? "" : " ·  n.s."}`) : h("span", { class: "d muted" }, isCtl ? "control" : ""));
      };
      return h("div", { class: "card rx-var" + (isCtl ? " ctl" : ""), style: { "--c": v.color } },
        h("div", { class: "rl-hd" }, h("span", { class: "rl-letter" }, v.name.slice(0, 2)), h("b", {}, "variant " + v.name),
          h("span", { class: "small muted" }, `${fmt.plural(v.reqs.length, "request")} · ${fmt.n(v.impressions)} shown`),
          h("span", { class: "sep" }), isCtl ? h("span", { class: "pill info" }, "control") : h("button", { class: "btn xs ghost", title: "Compare the other variants with this one", onclick: () => { st.control = v.name; save(); draw(); } }, "Make control")),
        h("div", { class: "rl-ms" }, METRICS.map(tile)),
        h("div", { class: "rx-vfoot small dim" },
          h("span", { title: "Share of the top k with a good label" }, "CTR ", h("b", {}, fmt.pct(v.m.ctr))),
          h("span", { title: "Distinct items shown" }, "items ", h("b", {}, fmt.n(v.coverage))),
          h("span", { title: "Share of what was shown that went to the top tenth of its items" }, "top 10% of items ", h("b", {}, fmt.pct(v.concentration))),
          h("span", { title: "How well the variant's own score tells good rows from the rest (AUC)" }, "score AUC ", h("b", {}, fmt.n(out.scoreAuc.find((x) => x.name === v.name)?.auc, 3)))));
    });
    const best = out.variants.filter((v) => v !== ctl).map((v) => ({ v, t: compare(ctl.ndcg, v.ndcg) })).filter((x) => x.t);
    const verdict = (() => {
      if (!best.length) return h("span", {}, "One variant: log a second ranking (\"variant\" in the log) to run an A/B test.");
      const wins = best.filter((x) => x.t.p < 0.05 && x.t.d > 0).sort((a, b) => b.t.d - a.t.d);
      const losses = best.filter((x) => x.t.p < 0.05 && x.t.d < 0);
      if (wins.length) { const w = wins[0]; return h("span", {}, icon("okc"), h("b", {}, ` Variant ${w.v.name} beats ${ctl.name}`), ` on nDCG@${st.k} by ${fmt.pct(w.t.lift)} (95% interval ${fmt.n(w.t.lo, 3)} to ${fmt.n(w.t.hi, 3)}, p ${w.t.p < 0.001 ? "< 0.001" : "= " + fmt.n(w.t.p, 3)}).`); }
      if (losses.length === best.length) return h("span", {}, icon("warn"), h("b", {}, ` The control ${ctl.name} is best`), `: every other variant is worse on nDCG@${st.k}, beyond chance.`);
      return h("span", {}, icon("clock"), h("b", {}, " No winner yet"), `: the differences in nDCG@${st.k} are within chance. More traffic narrows them.`);
    })();
    return h("div", { class: "rx-board" }, h("div", { class: "rx-verdict" }, verdict), h("div", { class: "rx-vars" }, cards));
  }

  function positionCard() {
    const series = out.variants.map((v) => ({ name: v.name, color: v.color, points: [...v.byPos.entries()].sort((a, b) => a[0] - b[0]).map(([p, b]) => [p + 1, b.n ? b.pos / b.n : null, `${fmt.n(b.pos)} of ${fmt.n(b.n)}`]) }));
    const max = Math.max(...out.variants.flatMap((v) => [...v.byPos.keys()])) + 1;
    const marks = out.maxShown && out.maxShown < max ? [{ x: out.maxShown + 0.5, label: "not shown →" }] : [];
    return h("div", { class: "card rx-wide" },
      h("div", { class: "hd" }, h("h3", {}, "Good rows by position"), h("span", { class: "sub" }, "Share of the rows at each position whose user acted on them. The fall from position 1 is what position bias and the ranking make together; rows past the line were logged as candidates but not shown")),
      h("div", { class: "bd" }, xyChart({ height: 220, series, marks, xmin: 1, xmax: max, xfmt: (x) => Number.isInteger(x) ? String(x) : "", yfmt: (y) => fmt.pct(y, 0), tipTitle: (x) => `position ${x}` })));
  }

  function featureCard() {
    const bar = (f) => {
      const a = f.auc, w = Math.abs(a - 0.5) * 2;
      return h("div", { class: "rx-feat", title: `AUC ${fmt.n(a, 3)} over ${fmt.n(f.n)} shown rows: ${a >= 0.5 ? "higher" : "lower"} values go with good rows` },
        h("code", { class: "ellipsis" }, f.name),
        h("div", { class: "rx-auc" }, h("span", { class: "mid" }), h("i", { style: { left: a >= 0.5 ? "50%" : (50 - w * 50) + "%", width: (w * 50) + "%", background: a >= 0.5 ? "var(--s1)" : "var(--s2)" } })),
        h("b", { class: "num" }, fmt.n(a, 2)));
    };
    return h("div", { class: "card rx-narrow" },
      h("div", { class: "hd" }, h("h3", {}, "Which values tell good rows apart"), h("span", { class: "sub" }, "AUC of each value the rankings read, as logged: 0.5 is chance, toward 1 higher is better, toward 0 lower is")),
      h("div", { class: "bd" }, out.feats.length ? h("div", { class: "rx-feats" }, h("div", { class: "rx-feat hdr small muted" }, h("span", {}, "value"), h("div", { class: "rx-auc-scale" }, h("span", {}, "0"), h("span", {}, "0.5"), h("span", {}, "1")), h("span", {}, "AUC")), out.feats.map(bar)) : empty("info", "No labelled values", "The rows need features and good labels both.")));
  }

  function timeCard() {
    const span = out.t1 - out.t0;
    const bucket = span > 2 * 86400e6 ? 86400e6 : span > 6 * 3600e6 ? 3600e6 : 600e6;
    const m = st.metric;
    const series = out.variants.map((v) => {
      const by = new Map();
      for (const q of v.reqs) { const b = Math.floor(q.ts / bucket) * bucket; const x = q[m]; if (x == null) continue; const a = by.get(b) || []; a.push(x); by.set(b, a); }
      return { name: v.name, color: v.color, points: [...by.entries()].sort((a, b) => a[0] - b[0]).map(([b, xs]) => [b / 1000, mean(xs), fmt.plural(xs.length, "request")]) };
    });
    const day = bucket >= 86400e6;
    const xfmt = (ms) => day ? new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" }) : fmt.time(ms, false);
    return h("div", { class: "card rx-wide" },
      h("div", { class: "hd" }, h("h3", {}, "Over time"), h("span", { class: "sub" }, `per ${day ? "day" : bucket >= 3600e6 ? "hour" : "10 minutes"}, by when the request was served`),
        h("div", { class: "tools" }, seg(METRICS.map(([k, l]) => [k, l]), m, (v) => { st.metric = v; save(); draw(); }))),
      h("div", { class: "bd" }, xyChart({ height: 200, series, xfmt, yfmt: (y) => m === "gain" ? fmt.n(y, 2) : fmt.n(y, 2), tipTitle: (x) => day ? new Date(x).toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" }) : fmt.datetime(x) })));
  }

  function segmentCard() {
    const ut = userTable();
    const fields = (ut?.short?.fields || []).filter((f) => f.type === "keyword" && !f.multi && f.name !== ut.short.key);
    const pick = h("select", { class: "select sm", onchange: async (e) => { st.segment = e.target.value; save(); await loadSegments(); draw(); } },
      h("option", { value: "" }, "(choose a field)"), fields.map((f) => h("option", { value: f.name, selected: f.name === st.segment }, f.name)));
    let bodyEl;
    if (!ut) bodyEl = empty("user", "No table of users", "When the events' user column REFERENCES a table, its fields break the measures down.");
    else if (!st.segment || !segments) bodyEl = empty("filter", "Break it down", `by a field of ${ut.table}: ${fields.map((f) => f.name).join(", ") || "none"}`);
    else {
      const groups = [...new Set([...segments.values()])].sort();
      const cell = (v, g) => {
        const xs = v.reqs.filter((q) => segments.get(String(q.context)) === g).map((q) => q.ndcg).filter((x) => x != null);
        const m = mean(xs);
        const lvl = m == null ? 0 : 1 + Math.min(6, Math.floor(m * 7));
        return h("td", { class: "num rx-heat", style: { background: m == null ? null : `var(--seq-${lvl})`, color: lvl >= 5 ? "var(--seq-1)" : null }, title: `${fmt.plural(xs.length, "request")} with a good row` }, m == null ? "–" : fmt.n(m, 3));
      };
      bodyEl = h("div", { class: "tbl-wrap" }, h("table", { class: "t compact" },
        h("thead", {}, h("tr", {}, h("th", {}, st.segment), out.variants.map((v) => h("th", { class: "num" }, h("span", { class: "rx-dot", style: { background: v.color } }), v.name)))),
        h("tbody", {}, groups.map((g) => h("tr", {}, h("td", {}, g), out.variants.map((v) => cell(v, g)))))));
    }
    return h("div", { class: "card rx-narrow" },
      h("div", { class: "hd" }, h("h3", {}, "nDCG by segment"), ut ? h("div", { class: "tools" }, pick) : null),
      h("div", { class: "bd" }, bodyEl));
  }

  function requestsCard() {
    const vs = new Map(out.variants.map((v) => [v.name, v]));
    let rows = out.requests.slice().sort((a, b) => b.ts - a.ts);
    if (st.only === "good") rows = rows.filter((q) => q.hit);
    else if (st.only === "none") rows = rows.filter((q) => !q.hit);
    const strip = (q) => h("span", { class: "rx-strip" }, q.shown.map((r) => h("i", { style: { background: labelFill(r.label) }, title: `${r.position + 1}. ${r.row_key}: label ${r.label ?? 0}` })));
    const tbl = h("div", { class: "tbl-wrap", style: { maxHeight: "420px" } }, h("table", { class: "t compact" },
      h("thead", {}, h("tr", {}, ["served", "context", "variant", `top ${st.k} (labels)`, "nDCG", "MRR"].map((c, i) => h("th", { class: i >= 4 ? "num" : "" }, c)))),
      h("tbody", {}, rows.slice(0, 300).map((q) => {
        const tr = h("tr", { class: "click", tabIndex: 0 },
          h("td", { class: "nw small" }, fmt.datetime(q.ts / 1000)), h("td", { class: "mono small" }, q.context ?? "–"),
          h("td", {}, h("span", { class: "rx-dot", style: { background: vs.get(q.variant)?.color } }), q.variant), h("td", {}, strip(q)),
          h("td", { class: "num" }, q.ndcg == null ? "–" : fmt.n(q.ndcg, 3)), h("td", { class: "num" }, fmt.n(q.rr, 2)));
        tr.onclick = () => openRequest(q); tr.onkeydown = (e) => { if (e.key === "Enter") openRequest(q); };
        return tr;
      }))));
    return h("div", { class: "card" },
      h("div", { class: "hd" }, h("h3", {}, "Requests"), h("span", { class: "sub" }, `${fmt.plural(rows.length, "request")}${rows.length > 300 ? ", the newest 300 listed" : ""}; open one to see what it served, what the user did before, and to grade its rows`),
        h("div", { class: "tools" }, seg([["", "all"], ["good", "with a good row"], ["none", "without"]], st.only, (v) => { st.only = v; save(); draw(); }))),
      h("div", { class: "bd flush" }, tbl),
      h("div", { class: "ft small muted rx-legend" }, "labels:", [0, 1, 2, 3].map((l) => h("span", {}, h("i", { style: { background: labelFill(l) } }), ["none", "1", "2", "3"][l]))));
  }

  // ------------------------------------------------------------ one request
  async function titles(keys) {
    const it = itemTable();
    if (!it) return;
    const shown = (it.short.fields || []).find((f) => f.type === "text" && f.stored !== false) || (it.short.fields || []).find((f) => f.type === "keyword" && f.name !== it.short.key);
    const want = keys.filter((k) => !itemTitles.has(k));
    if (!shown || !want.length) return;
    try {
      const j = await api("/v1/sql", { body: { sql: `SELECT ${quoteName(it.short.key)}, ${quoteName(shown.name)} FROM ${quoteName(it.table)} WHERE ${quoteName(it.short.key)} IN (${want.map((k) => `'${String(k).replace(/'/g, "''")}'`).join(", ")}) LIMIT ${want.length}` } });
      for (const [k, t] of j.rows) itemTitles.set(String(k), t);
    } catch { /* keys only */ }
  }

  async function openRequest(q) {
    await titles([...q.rows.map((r) => r.row_key), ...q.history.slice(-10)]);
    const v = out.variants.find((x) => x.name === q.variant);
    const feats = [...new Set(q.rows.flatMap((r) => Object.keys(r.features || {})))];
    const list = h("div", { class: "rx-served" });
    const drawList = () => clear(list, q.rows.slice().sort((a, b) => a.position - b.position).map((r) => h("div", { class: "rx-srow" + (r.shown ? "" : " unshown") },
      h("span", { class: "rl-n" }, r.position + 1),
      h("div", { class: "rl-t" }, h("div", { class: "ellipsis" }, itemTitles.get(r.row_key) || r.row_key), h("div", { class: "small muted mono" }, r.row_key, r.shown ? "" : " · not shown", " · score ", fmt.n(r.score, 3),
        feats.length ? h("span", { class: "rx-fchips" }, feats.slice(0, 3).map((f) => h("span", { class: "rx-fchip", title: f }, f.length > 24 ? f.slice(0, 22) + "…" : f, " ", h("b", {}, fmt.n(r.features?.[f], 2))))) : null)),
      h("span", { class: "rx-glbl", style: { background: labelFill(r.label), color: (r.label || 0) >= 2 ? "var(--seq-1)" : null }, title: "label" }, r.label ?? 0),
      h("span", { class: "rx-gbtns" }, [0, 1, 2, 3].map((g) => h("button", { class: "btn xs ghost", title: `Write label ${g} on this row of the log`, onclick: () => grade(r, g) }, g))))));
    async function grade(r, g) {
      try {
        await api("/v1/sql", { body: { sql: `UPDATE ${quoteName(st.log)} SET label = ${g} WHERE id = '${String(r.id).replace(/'/g, "''")}'` } });
        r.label = g; drawList(); toast(`Label ${g} written on ${r.row_key}; it wins over the events' label`, "ok");
      } catch (e) { toast("Not written: " + e.message, "bad"); }
    }
    drawList();
    const hist = q.history.slice(-10).map((k, i) => h("span", { class: "chip", title: itemTitles.get(k) || k }, h("span", { class: "muted" }, q.historyActions.slice(-10)[i] || ""), " ", (itemTitles.get(k) || k).slice(0, 34)));
    drawer({
      title: `Request ${String(q.id).slice(0, 12)}…`, sub: `${q.context ?? "no context"} · variant ${q.variant} · ${fmt.datetime(q.ts / 1000)}`, wide: true,
      body: h("div", { class: "stack" },
        h("div", { class: "rl-ms", style: { "--c": v?.color } }, [["nDCG", q.ndcg == null ? "–" : fmt.n(q.ndcg, 3)], ["hit", q.hit ? "yes" : "no"], ["MRR", fmt.n(q.rr, 2)], ["gain", fmt.n(q.gain, 1)]].map(([l, x]) => h("div", { class: "rl-m" }, h("span", { class: "lbl" }, l), h("b", {}, x)))),
        q.history.length ? h("div", {}, h("div", { class: "small muted", style: { marginBottom: "6px" } }, "What the user did before, oldest first (the history a sequence model trains on)"), h("div", { class: "row wrap", style: { gap: "6px" } }, hist)) : null,
        h("div", { class: "small muted" }, `What it served: ${fmt.plural(q.rows.length, "row")} logged, ${fmt.n(q.rows.filter((r) => r.shown).length)} shown. A label written here wins over the one the events give.`),
        list),
    });
  }

  return () => aborter?.abort();
}
