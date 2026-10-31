// Autocomplete: a search box completing what is typed from a KEYWORD
// field, one query per keystroke (`PREFIX(field, :p)`, with typos if
// asked), the most frequent first. Each keystroke's time, the rows its
// prefix matched and how the plan read them are listed, and a measure by
// prefix length runs the table's own values cut at one to ten characters.

import { h, icon, clear, fmt, api, store, empty, seg, failure, sqlBlock, toggle, debounce } from "../core.js";
import { tables, table as tableOf } from "../catalog.js";
import { xyChart, SERIES } from "../charts.js";
import { quoteName } from "../sqledit.js";

export default { id: "autocomplete", title: "Autocomplete", icon: "autocomplete", group: "Explore", full: true, keywords: "autocomplete typeahead suggest prefix complete search as you type typos fuzzy query completion keystroke latency", render };

const TYPOS = [["off", "exact"], ["1", "1 typo"], ["2", "2 typos"], ["auto", "auto"]];
const lit = (v) => `'${String(v).replace(/'/g, "''")}'`;

/** The keyword fields a box can complete, per table. */
const kwFields = (t) => (t?.short?.fields || []).filter((f) => f.type === "keyword" && f.indexed !== false);
const numFields = (t) => (t?.short?.fields || []).filter((f) => f.type === "int" || f.type === "float");

function guess(ts) {
  const score = (t) => {
    const key = t.short?.key, kf = kwFields(t).find((f) => f.name === key);
    return (kf ? 2 : 0) + (/quer|search|sugg|complet|term/i.test(t.table) ? 3 : 0) + (numFields(t).length ? 1 : 0);
  };
  const t = ts.filter((x) => kwFields(x).length).sort((a, b) => score(b) - score(a))[0];
  if (!t) return null;
  const f = kwFields(t).find((x) => x.name === t.short.key) || kwFields(t)[0];
  const n = numFields(t).find((x) => /^(n|count|clicks|hits|freq|popularity|sold|views)$/i.test(x.name)) || numFields(t)[0];
  return { table: t.table, field: f.name, order: n?.name || "", typos: "auto", exactFirst: true, k: 8, where: "" };
}

function render(page) {
  let ts = [], st = store.get("ac.state", null), seq = 0, sugg = [], active = -1, hist = [], bench = null, benchRun = 0;
  const save = () => store.set("ac.state", st);
  const top = h("div", { class: "ac-top" });
  const main = h("div", { class: "ac-main" });
  const side = h("div", { class: "ac-side" });
  page.append(h("div", { class: "ac" }, top, h("div", { class: "ac-grid" }, main, side)));

  tables().then((list) => {
    ts = list;
    if (!st || !tableOf(st.table) || !kwFields(tableOf(st.table)).some((f) => f.name === st.field)) st = guess(ts);
    if (!st) { clear(main, empty("autocomplete", "Nothing to complete", "Autocomplete reads a KEYWORD field: a table of queries (id KEYWORD PRIMARY KEY, n INT) is the usual one.")); return; }
    save(); drawTop(); drawMain(); drawSide();
  }).catch((e) => clear(main, failure(e, "The tables")));

  const t = () => tableOf(st.table);
  const grouped = () => st.field !== t().short.key;

  /** The query for a prefix, exact or with typos. */
  function sqlFor(typos) {
    const T = quoteName(st.table), F = quoteName(st.field);
    const fz = typos && typos !== "off" ? `, fuzzy => ${typos === "auto" ? "'auto'" : typos}` : "";
    const where = `PREFIX(${F}, :p${fz})${st.where.trim() ? ` AND (${st.where})` : ""}`;
    if (grouped()) return `SELECT ${F}, COUNT(*) AS n FROM ${T} WHERE ${where} GROUP BY ${F} ORDER BY n DESC LIMIT ${st.k}`;
    const O = st.order ? quoteName(st.order) : "";
    return `SELECT ${F}${O ? ", " + O : ""} FROM ${T} WHERE ${where}${O ? ` ORDER BY ${O} DESC, ${F}` : ""} LIMIT ${st.k}`;
  }
  async function ask(p, typos) {
    const t0 = performance.now();
    const j = await api("/v1/query", { body: { sql: sqlFor(typos), params: { p } } });
    const rows = j.rows.map((r) => ({ value: String(r[0]), n: r.length > 1 ? r[1] : null }))
      .filter((r) => !grouped() || !(tableOf(st.table).short.fields.find((f) => f.name === st.field)?.multi) || typos !== "off" || r.value.startsWith(p));
    return { rows, took: j.took_ms, rtt: performance.now() - t0, matched: j.stats?.filter_docs, plan: j.stats?.plan || [] };
  }

  // ------------------------------------------------------------ controls
  function drawTop() {
    const tt = t();
    clear(top,
      h("label", { class: "rx-lbl" }, "complete", h("select", { class: "select sm", onchange: (e) => { st = { ...guess([tableOf(e.target.value)]), typos: st.typos, exactFirst: st.exactFirst }; save(); drawTop(); drawMain(); drawSide(); } }, ts.filter((x) => kwFields(x).length).map((x) => h("option", { value: x.table, selected: x.table === st.table }, x.table)))),
      h("label", { class: "rx-lbl" }, "field", h("select", { class: "select sm mono", onchange: (e) => { st.field = e.target.value; save(); drawMain(); drawSide(); } }, kwFields(tt).map((f) => h("option", { value: f.name, selected: f.name === st.field }, f.name + (f.multi ? "[]" : ""))))),
      grouped() ? h("span", { class: "small muted", title: "The field is not the key, so completions are its values, counted" }, "values by how many rows have them") :
        h("label", { class: "rx-lbl" }, "most first by", h("select", { class: "select sm mono", onchange: (e) => { st.order = e.target.value; save(); drawMain(); drawSide(); } }, h("option", { value: "" }, "(none)"), numFields(tt).map((f) => h("option", { value: f.name, selected: f.name === st.order }, f.name)))),
      h("label", { class: "rx-lbl" }, "typos", seg(TYPOS, st.typos, (v) => { st.typos = v; save(); drawTop(); refresh(); drawSide(); })),
      st.typos !== "off" ? toggle(st.exactFirst, (v) => { st.exactFirst = v; save(); refresh(); }, "exact first") : null,
      h("label", { class: "rx-lbl" }, "show", seg([[5, "5"], [8, "8"], [10, "10"]], st.k, (v) => { st.k = v; save(); refresh(); drawSide(); })),
      h("input", { class: "input sm mono", style: { width: "220px" }, placeholder: "and WHERE (optional): lang = 'en'", value: st.where, oninput: debounce((e) => { st.where = e.target.value; save(); refresh(); drawSide(); }, 400) }));
  }

  // ------------------------------------------------------------ the box
  let input, ghost, list, timeline, status;
  function drawMain() {
    input = h("input", { class: "ac-input", placeholder: `Start typing a ${st.field}…`, autocomplete: "off", spellcheck: false, oninput: () => { active = -1; refresh(); }, onkeydown: keys });
    ghost = h("div", { class: "ac-ghost", "aria-hidden": "true" });
    list = h("div", { class: "ac-list", role: "listbox" });
    status = h("div", { class: "ac-status small muted" });
    timeline = h("div", { class: "ac-tl" });
    hist = [];
    clear(main,
      h("div", { class: "card ac-demo" },
        h("div", { class: "ac-box" }, icon("search"), h("div", { class: "ac-field" }, ghost, input), h("kbd", { class: "small muted", title: "Tab takes the grey completion; arrows and Enter pick one" }, "Tab")),
        list, status),
      h("div", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Each keystroke"), h("span", { class: "sub" }, "newest first: the server's time for the query, the round trip, and how many rows the prefix matched")), h("div", { class: "bd flush" }, timeline)));
    drawTimeline();
    setTimeout(() => input.focus(), 30);
  }

  function keys(e) {
    if (e.key === "ArrowDown") { e.preventDefault(); active = Math.min(sugg.length - 1, active + 1); drawList(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); active = Math.max(-1, active - 1); drawList(); }
    else if (e.key === "Enter" && active >= 0) { e.preventDefault(); pick(sugg[active].value); }
    else if (e.key === "Tab" && sugg[0] && sugg[0].value.startsWith(input.value) && sugg[0].value !== input.value) { e.preventDefault(); pick(sugg[0].value); }
    else if (e.key === "Escape") { sugg = []; drawList(); }
  }
  function pick(v) { input.value = v; active = -1; refresh(); input.focus(); }

  async function refresh() {
    if (!input) return;
    const p = input.value;
    const mine = ++seq;
    if (!p) { sugg = []; drawList(); clear(status); return; }
    try {
      const fuzzy = st.typos !== "off";
      const first = await ask(p, fuzzy && !st.exactFirst ? st.typos : "off");
      let rows = first.rows.map((r) => ({ ...r, typo: !r.value.startsWith(p) }));
      let second = null;
      if (fuzzy && st.exactFirst && rows.length < st.k) {
        second = await ask(p, st.typos);
        const seen = new Set(rows.map((r) => r.value));
        rows = rows.concat(second.rows.filter((r) => !seen.has(r.value)).map((r) => ({ ...r, typo: !r.value.startsWith(p) }))).slice(0, st.k);
      }
      if (mine !== seq) return;
      sugg = rows;
      const took = first.took + (second?.took || 0), rtt = first.rtt + (second?.rtt || 0);
      hist.unshift({ p, took, rtt, matched: first.matched, matched2: second?.matched, n: rows.length, typos: rows.filter((r) => r.typo).length, plan: [...first.plan, ...(second?.plan || [])], two: !!second });
      hist = hist.slice(0, 24);
      drawList(); drawTimeline();
      clear(status, `${fmt.plural(rows.length, "completion")} in ${fmt.ms(took)} on the server${second ? " (exact, then with typos)" : ""}, ${fmt.ms(rtt)} round trip · ${fmt.n(first.matched ?? 0)} rows start with “${p}”`);
      drawPlan(hist[0]);
    } catch (e) {
      if (mine !== seq) return;
      sugg = []; drawList(); clear(status, h("span", { style: { color: "var(--crit-ink)" } }, e.message));
    }
  }

  function drawList() {
    const p = input.value;
    const top0 = sugg[0];
    ghost.replaceChildren(h("span", { class: "ac-gin" }, p), top0 && !top0.typo && top0.value.startsWith(p) ? top0.value.slice(p.length) : "");
    const max = Math.max(1, ...sugg.map((r) => r.n || 0));
    clear(list, sugg.map((r, i) => {
      const k = r.typo ? 0 : p.length;
      const el = h("div", { class: "ac-item" + (i === active ? " on" : ""), role: "option", "aria-selected": String(i === active), onmousedown: (e) => { e.preventDefault(); pick(r.value); }, onmouseenter: () => { active = i; drawList(); } },
        icon(r.typo ? "sparkle" : "search"),
        h("span", { class: "ac-text" }, h("span", { class: "ac-typed" }, r.value.slice(0, k)), h("b", {}, r.value.slice(k))),
        r.typo ? h("span", { class: "pill info", title: "Found with a typo allowed: it starts with something within the edits of what was typed" }, "typo") : null,
        r.n != null ? h("span", { class: "ac-n" }, h("span", { class: "ac-bar" }, h("i", { style: { width: (r.n / max * 100) + "%" } })), h("span", { class: "num small dim" }, fmt.si(r.n))) : null);
      return el;
    }));
  }

  function drawTimeline() {
    if (!timeline) return;
    if (!hist.length) { clear(timeline, h("div", { class: "small muted", style: { padding: "14px" } }, "Type in the box: every keystroke is one query.")); return; }
    const max = Math.max(...hist.map((x) => x.rtt), 0.5);
    clear(timeline, hist.map((x, i) => h("div", { class: "ac-ks" + (i === 0 ? " now" : "") },
      h("code", { class: "ac-p" }, x.p.length > 28 ? "…" + x.p.slice(-27) : x.p, h("span", { class: "ac-caret" })),
      h("div", { class: "ac-lat", title: `server ${fmt.ms(x.took)}, round trip ${fmt.ms(x.rtt)}` }, h("i", { class: "rtt", style: { width: (x.rtt / max * 100) + "%" } }), h("i", { class: "srv", style: { width: (x.took / max * 100) + "%" } })),
      h("span", { class: "num small" }, h("b", {}, fmt.ms(x.took)), h("span", { class: "muted" }, " / " + fmt.ms(x.rtt))),
      h("span", { class: "num small dim", title: "rows the prefix matched" }, fmt.si(x.matched ?? 0), " rows"),
      x.typos ? h("span", { class: "pill info" }, `${x.typos} by typo`) : h("span", {}))));
  }

  // ------------------------------------------------------------ the side
  let planEl, chartBox;
  function drawSide() {
    const tt = t();
    const clustered = tt.short.cluster_by === st.field;
    planEl = h("div", { class: "ac-plan" });
    chartBox = h("div", {});
    clear(side,
      h("div", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "The query"), h("div", { class: "tools" }, h("a", { class: "btn sm ghost", href: "#/sql?q=" + encodeURIComponent(sqlFor(st.typos === "off" ? "off" : st.typos)) + "&p=" + encodeURIComponent(JSON.stringify({ p: "qui" })), title: "Open it in the SQL workbench" }, icon("play"), "Workbench"))),
        h("div", { class: "bd" }, sqlBlock(sqlFor(st.typos === "off" || !st.exactFirst ? st.typos : "off")), st.typos !== "off" && st.exactFirst ? h("div", { class: "small muted", style: { marginTop: "6px" } }, "then, when it finds fewer than " + st.k + ", the same with ", h("code", {}, st.typos === "auto" ? "fuzzy => 'auto'" : `fuzzy => ${st.typos}`), " fills the rest") : null)),
      h("div", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "How it reads the table")),
        h("div", { class: "bd stack" },
          h("div", { class: "small dim" }, `${st.field}'s values are sorted in its dictionary, so the values that start with what was typed are one run of it, found by two binary searches per segment.${st.order && !grouped() ? ` ORDER BY ${st.order} DESC reads ${st.order}'s blocks best first and stops after ${st.k} rows.` : ""}${st.typos !== "off" ? " Typos walk the dictionary as a trie, skipping every start already too far from what was typed." : ""}`),
          grouped() ? null : clustered ? h("div", { class: "row nw small" }, h("span", { class: "pill ok" }, "clustered"), h("span", { class: "dim" }, `by ${st.field}: a prefix's rows sit together, so the top ${st.k} read about ${st.k} blocks whatever the prefix`))
            : h("div", { class: "small" }, h("span", { class: "pill warn" }, "not clustered"), h("span", { class: "dim" }, ` Keeping ${st.table} clustered by ${st.field} makes one-letter prefixes as fast as long ones: `), h("code", {}, `OPTIONS(cluster_by = '${st.field}')`)),
          planEl)),
      h("div", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "By prefix length"), h("span", { class: "sub" }, "the table's own values cut at 1 to 10 characters"), h("div", { class: "tools" }, h("button", { class: "btn sm", onclick: measure }, icon("gauge"), bench ? "Again" : "Measure"))),
        h("div", { class: "bd" }, chartBox)));
    drawPlan(hist[0]);
    drawBench();
  }
  function drawPlan(x) {
    if (!planEl) return;
    clear(planEl, x ? [h("div", { class: "small muted" }, `The plan of “${x.p}”`), h("pre", { class: "code small" }, x.plan.slice(0, 8).join("\n") + (x.plan.length > 8 ? `\n… ${x.plan.length - 8} more` : ""))] : []);
  }

  async function measure() {
    const mine = ++benchRun;
    bench = { lengths: [], running: true };
    drawSide();
    const F = quoteName(st.field), T = quoteName(st.table);
    let values = [];
    try {
      const j = await api("/v1/sql", { body: { sql: `SELECT ${F} FROM ${T}${st.order && !grouped() ? ` ORDER BY ${quoteName(st.order)} DESC` : ""} LIMIT 400` } });
      values = [...new Set(j.rows.map((r) => [].concat(r[0])[0]).filter((v) => typeof v === "string" && v.length >= 3))];
    } catch (e) { clear(chartBox, failure(e, "The values")); return; }
    for (let i = values.length - 1; i > 0; i--) { const j = Math.floor(Math.random() * (i + 1)); [values[i], values[j]] = [values[j], values[i]]; }
    values = values.slice(0, 30);
    const modes = st.typos === "off" ? ["off"] : ["off", st.typos];
    const res = new Map();
    for (let L = 1; L <= 10; L++) {
      for (const m of modes) {
        const xs = [], ns = [];
        for (const v of values) {
          if (v.length < L) continue;
          if (mine !== benchRun) return;
          try { const r = await ask(v.slice(0, L), m); xs.push(r.took); ns.push(r.matched ?? 0); } catch { /* skipped */ }
        }
        xs.sort((a, b) => a - b); ns.sort((a, b) => a - b);
        const q = (a, f) => a.length ? a[Math.min(a.length - 1, Math.floor(f * a.length))] : null;
        res.set(`${m}:${L}`, { p50: q(xs, 0.5), p95: q(xs, 0.95), rows: q(ns, 0.5), n: xs.length });
      }
      bench = { res, modes, done: L, running: L < 10 };
      drawBench();
    }
  }
  function drawBench() {
    if (!chartBox) return;
    if (!bench) { clear(chartBox, h("div", { class: "small muted" }, "Runs 30 of the table's values cut at each length, exact and with the typos chosen, and draws the server's median and 95th percentile per length.")); return; }
    if (!bench.res) { clear(chartBox, h("div", { class: "rx-wait" }, h("span", { class: "spin" }), "Measuring…")); return; }
    const series = bench.modes.flatMap((m, i) => [
      { name: (m === "off" ? "exact" : `typos ${m}`) + " p50", color: SERIES[i * 2], points: [...Array(bench.done)].map((_, k) => [k + 1, bench.res.get(`${m}:${k + 1}`)?.p50, `${fmt.si(bench.res.get(`${m}:${k + 1}`)?.rows)} rows matched (median)`]) },
      { name: (m === "off" ? "exact" : `typos ${m}`) + " p95", color: SERIES[i * 2], dash: "4 3", points: [...Array(bench.done)].map((_, k) => [k + 1, bench.res.get(`${m}:${k + 1}`)?.p95]) },
    ]);
    clear(chartBox, xyChart({ height: 210, series, xmin: 1, xmax: 10, xfmt: (x) => String(x), yfmt: (y) => fmt.ms(y), tipTitle: (x) => `${x} character${x === 1 ? "" : "s"} typed` }),
      bench.running ? h("div", { class: "small muted" }, h("span", { class: "spin" }), ` ${bench.done} of 10 lengths`) : null);
  }

  return () => { benchRun++; };
}
