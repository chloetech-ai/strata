// The relevance lab: two rankings of one query side by side, the lines
// between them showing how each row moved, grades given to the rows (kept
// in this browser) and the measures they give each ranking: nDCG, the
// share of good rows, the first good row. Claude can grade the rows both
// rankings show (the pool) with UMBRELA's prompt (/v1/judge), and the
// measures then read your grades, Claude's, or yours with Claude's where
// you gave none; where both graded a row, how often they agree is shown. A
// set of queries with grades compares the rankings over all of them, and
// a weighted hybrid's weight (or rank fusion's k) is tuned by trying each
// over the set. The ranking that wins can be kept with its table as a
// rank profile.
//
// A weighted hybrid normalises each score with tmm() by default: theoretical
// min-max, which Bruch, Gai and Ingber (TOIS 2023) find fuses better than
// RRF once its one weight is tuned on a few graded queries; minmax() and
// zscore() are there to compare.

import { h, s, icon, clear, fmt, api, store, toast, modal, empty, seg, sqlBlock, debounce, failure, showTip, hideTip } from "../core.js";
import { tables, table as tableOf, invalidate } from "../catalog.js";
import { quoteName } from "../sqledit.js";

export default { id: "relevance", title: "Relevance lab", icon: "relevance", group: "Tune", full: true, keywords: "relevance ranking compare a/b judgments ndcg mrr precision tune weights hybrid fusion rrf bm25 vector rank profile evaluation", render };

const PRESETS = [
  ["bm25", "Words (BM25)"], ["fuzzy", "Words, typos allowed"], ["vector", "Meaning (vectors)"], ["weighted", "Hybrid: weighted"],
  ["rrf", "Hybrid: rank fusion"], ["boost", "Words boosted by a number"], ["profile", "A rank profile"], ["custom", "An expression"],
];
// How a weighted hybrid puts each score on one scale.
const NORMS = [["tmm", "tmm"], ["minmax", "minmax"], ["zscore", "zscore"]];
const RRF_KS = [1, 5, 10, 20, 40, 60, 100];
const COLORS = ["var(--s1)", "var(--s2)"];
const GRADES = [[0, "bad"], [1, "fair"], [2, "good"], [3, "perfect"]];
/** Whose grades the measures read. */
const SOURCES = [["mine", "yours"], ["claude", "Claude's"], ["both", "yours, else Claude's"]];
const ROW_H = 48;

/** nDCG@k, the share of good rows (grade ≥ 2) in the top k, the reciprocal rank of the first good row, and how many of the top k are graded. */
export function measures(keys, grades, k = 10) {
  const top = keys.slice(0, k);
  const g = (key) => grades[key] ?? null;
  const dcg = top.reduce((a, key, i) => a + ((2 ** (g(key) ?? 0)) - 1) / Math.log2(i + 2), 0);
  const ideal = Object.values(grades).sort((a, b) => b - a).slice(0, k).reduce((a, x, i) => a + ((2 ** x) - 1) / Math.log2(i + 2), 0);
  const firstGood = top.findIndex((key) => (g(key) ?? 0) >= 2);
  return {
    ndcg: ideal > 0 ? dcg / ideal : null,
    precision: top.length ? top.filter((key) => (g(key) ?? 0) >= 2).length / k : null,
    mrr: firstGood >= 0 ? 1 / (firstGood + 1) : 0,
    judged: top.filter((key) => g(key) != null).length,
    n: top.length,
  };
}

/** How two graders agree on the rows both graded: the rows, the share given the same grade, within one, and Cohen's kappa with linear weights. */
export function agreement(a, b) {
  const both = Object.keys(a).filter((k) => b[k] != null);
  const n = both.length;
  if (!n) return { n: 0, exact: null, withinOne: null, kappa: null };
  const exact = both.filter((k) => a[k] === b[k]).length / n;
  const withinOne = both.filter((k) => Math.abs(a[k] - b[k]) <= 1).length / n;
  // Weighted kappa: 1 - observed disagreement / disagreement expected from each grader's own grades.
  const pa = [0, 0, 0, 0], pb = [0, 0, 0, 0];
  for (const k of both) { pa[a[k]]++; pb[b[k]]++; }
  const w = (i, j) => Math.abs(i - j) / 3;
  const obs = both.reduce((x, k) => x + w(a[k], b[k]), 0) / n;
  let exp = 0;
  for (let i = 0; i < 4; i++) for (let j = 0; j < 4; j++) exp += (pa[i] / n) * (pb[j] / n) * w(i, j);
  return { n, exact, withinOne, kappa: exp > 0 ? 1 - obs / exp : null };
}

function render(page) {
  let ts = [];
  let st = store.get("rel.state", null);
  let results = [null, null], running = 0, evalOut = null;
  const save = debounce(() => store.set("rel.state", st), 300);
  const allGrades = () => store.get("rel.grades", {});
  const gradesFor = (q = st.q) => allGrades()[st.table]?.[q.trim().toLowerCase()] || {};
  const setGrade = (key, v) => {
    const all = allGrades(); const t = (all[st.table] ||= {}); const qq = (t[st.q.trim().toLowerCase()] ||= {});
    if (v == null) delete qq[key]; else qq[key] = v;
    store.set("rel.grades", all); drawCompare();
  };
  // Claude's grades, kept apart from yours: {table: {query: {key: grade}}}.
  const allLlm = () => store.get("rel.llm", {});
  const llmFor = (q = st.q) => allLlm()[st.table]?.[q.trim().toLowerCase()] || {};
  const addLlm = (q, got) => {
    const all = allLlm(); const t = (all[st.table] ||= {}); Object.assign((t[q.trim().toLowerCase()] ||= {}), got);
    store.set("rel.llm", all);
  };
  const source = () => st.source || "both";
  /** The grades the measures read, by the source chosen. */
  const measured = (q = st.q) => source() === "mine" ? gradesFor(q) : source() === "claude" ? llmFor(q) : { ...llmFor(q), ...gradesFor(q) };
  const querySet = () => {
    const qs = new Set();
    if (source() !== "claude") for (const [q, g] of Object.entries(allGrades()[st.table] || {})) if (Object.keys(g).length) qs.add(q);
    if (source() !== "mine") for (const [q, g] of Object.entries(allLlm()[st.table] || {})) if (Object.keys(g).length) qs.add(q);
    return [...qs];
  };
  let judging = false;

  const top = h("div", { class: "rl-top" });
  const setBar = h("div", { class: "rl-set" });
  const cmp = h("div", { class: "rl-cmp" });
  const evalBox = h("div", { class: "rl-eval" });
  page.append(h("div", { class: "rl" }, top, setBar, cmp, evalBox));

  tables().then((list) => {
    ts = list;
    if (!ts.length) { clear(cmp, empty("relevance", "No tables yet", "Import a file first.")); return; }
    if (!st || !tableOf(st.table)) st = fresh(ts[0]);
    drawTop(); drawSet(); run();
  }).catch((e) => clear(cmp, failure(e, "The tables")));

  const fields = () => tableOf(st.table)?.short?.fields || [];
  const textF = () => fields().filter((f) => f.type === "text" && f.indexed !== false);
  const vecF = () => fields().filter((f) => f.type === "vector");
  const numF = () => fields().filter((f) => f.type === "int" || f.type === "float");
  function fresh(t) {
    const fs = t.short?.fields || [];
    const tf = fs.find((f) => f.type === "text" && f.indexed !== false)?.name, vf = fs.find((f) => f.type === "vector")?.name, nf = fs.find((f) => f.type === "int" || f.type === "float")?.name;
    const shown = fs.find((f) => f.type === "text" && f.stored !== false)?.name || t.short.key;
    return { table: t.table, shown, q: "", where: "", k: 10,
      rankings: [{ preset: "bm25", field: tf, vfield: vf, nfield: nf, alpha: 0.3, norm: "tmm", k: 60, w: 0.1, custom: "" }, { preset: vf ? "weighted" : "fuzzy", field: tf, vfield: vf, nfield: nf, alpha: 0.3, norm: "tmm", k: 60, w: 0.1, custom: "" }] };
  }

  /** The ORDER BY of a ranking, and whether it reads the embedded text (:v). */
  function expr(r) {
    const F = quoteName(r.field || ""), V = quoteName(r.vfield || ""), N = quoteName(r.nfield || "");
    const a = Math.round((r.alpha ?? 0.5) * 100) / 100;
    switch (r.preset) {
      case "bm25": return `bm25(${F}, :q)`;
      case "fuzzy": return `bm25(${F}, :q, fuzzy => 'auto')`;
      case "vector": return `similarity(${V}, :v)`;
      case "rrf": return `rrf(bm25(${F}, :q), similarity(${V}, :v)${(r.k ?? 60) !== 60 ? `, k => ${r.k}` : ""})`;
      case "weighted": { const n = r.norm || "tmm"; return `${a} * ${n}(bm25(${F}, :q)) + ${Math.round((1 - a) * 100) / 100} * ${n}(similarity(${V}, :v))`; }
      case "boost": return `bm25(${F}, :q) * (1 + ${r.w ?? 0.1} * coalesce(${N}, 0))`;
      case "profile": return `rank_profile('${(r.profile || "").replace(/'/g, "''")}', q => :q${/:v\b/.test(profileSql(r.profile)) ? ", v => :v" : ""})`;
      default: return r.custom || "";
    }
  }
  const profileSql = (name) => { const p = (tableOf(st.table)?.profiles || []).find((x) => x.name === name); return p ? JSON.stringify(p) : ""; };
  function sqlOf(r, q = st.q, over) {
    const t = tableOf(st.table);
    const key = t.short.key;
    const e = expr(over ? { ...r, ...over } : r);
    const cols = [quoteName(key), st.shown && st.shown !== key ? quoteName(st.shown) : null, "score()", r.preset === "custom" ? null : "features()"].filter(Boolean).join(", ");
    const sql = `SELECT ${cols} FROM ${quoteName(t.table)}${st.where.trim() ? ` WHERE ${st.where}` : ""} ORDER BY ${e}${r.preset === "profile" ? "" : " DESC"} LIMIT ${st.k}`;
    const params = { q };
    if (/:v\b/.test(e)) params.v = r.vfield ? { embed: q, field: r.vfield } : { embed: q };
    return { sql, params };
  }
  async function query(r, q, over) {
    const { sql, params } = sqlOf(r, q, over);
    const j = await api("/v1/query", { body: { sql, params } });
    const t = tableOf(st.table);
    const ki = j.columns.indexOf(t.short.key), ti = st.shown && st.shown !== t.short.key ? j.columns.indexOf(st.shown) : -1, si = j.columns.indexOf("score()");
    const fi = j.columns.map((c, i) => i).filter((i) => i !== ki && i !== ti && i !== si);
    return { sql, took: j.took_ms, rows: j.rows.map((row) => ({ key: String(row[ki]), title: ti >= 0 ? row[ti] : "", score: row[si], feats: Object.fromEntries(fi.map((i) => [j.columns[i], row[i]])) })) };
  }

  // ------------------------------------------------------------ the top bar
  function drawTop() {
    const t = tableOf(st.table);
    const qIn = h("input", { class: "input rl-q", value: st.q, placeholder: "A query to judge, such as quiet charger", oninput: (e) => { st.q = e.target.value; save(); runSoon(); } });
    const where = h("input", { class: "input sm mono", value: st.where, placeholder: "WHERE (optional): price < 100", style: { width: "240px" }, oninput: (e) => { st.where = e.target.value; save(); runSoon(); } });
    clear(top,
      h("select", { class: "select", title: "The table", onchange: (e) => { st = fresh(tableOf(e.target.value)); save(); drawTop(); drawSet(); run(); } }, ts.map((x) => h("option", { value: x.table, selected: x.table === st.table }, x.table))),
      h("span", { class: "rl-qwrap" }, icon("search"), qIn),
      where,
      h("label", { class: "row nw small dim", title: "The field each row shows" }, "show", h("select", { class: "select sm", onchange: (e) => { st.shown = e.target.value; save(); run(); } }, (t.short.fields || []).filter((f) => f.stored !== false && f.type !== "vector").map((f) => h("option", { value: f.name, selected: f.name === st.shown }, f.name)))),
      h("label", { class: "row nw small dim" }, "top", seg([[10, "10"], [20, "20"], [50, "50"]], st.k, (v) => { st.k = v; save(); run(); })));
    setTimeout(() => { if (!st.q) qIn.focus(); }, 30);
  }
  function drawSet() {
    const qs = querySet();
    const cur = st.q.trim().toLowerCase();
    clear(setBar, h("span", { class: "small muted" }, qs.length ? "Graded queries" : "Grade a few rows of a query to start a set, or let Claude grade them; the set compares the rankings over all of them."),
      qs.map((q) => { const n = Object.keys(measured(q)).length; return h("button", { class: "chip", "aria-pressed": String(q === cur), title: `${fmt.plural(n, "grade")}`, onclick: () => { st.q = q; save(); drawTop(); run(); } }, q, h("span", { class: "muted" }, n)); }),
      h("span", { class: "sep" }),
      h("label", { class: "row nw small dim", title: "Whose grades the measures read" }, "grades", seg(SOURCES, source(), (v) => { st.source = v; save(); drawSet(); drawCompare(); })),
      h("button", { class: "btn sm", disabled: judging || !st.q.trim(), title: "Claude grades the rows both rankings show for this query (UMBRELA's prompt), each on its own", onclick: () => judge([st.q]) }, icon("agent"), judging ? "Grading…" : "Grade with Claude"),
      qs.length ? h("button", { class: "btn sm primary", onclick: evaluate }, icon("relevance"), `Compare over ${fmt.plural(qs.length, "query", "queries")}`) : null);
  }

  // ------------------------------------------------------------ Claude's grades
  /** The server's key, or one entered here (kept in this browser only if asked). */
  async function apiKey() {
    const cfg = await api("/v1/config").catch(() => ({}));
    if (cfg.agent?.server_key) return { key: undefined, model: cfg.agent.model };
    const kept = store.get("judge.key", "");
    if (kept) return { key: kept, model: cfg.agent?.model };
    return new Promise((resolve) => {
      const k = h("input", { class: "input mono", type: "password", placeholder: "sk-ant-…", style: { width: "100%" } });
      const keep = h("input", { type: "checkbox" });
      const done = (v) => { m.close(); resolve(v); };
      const m = modal({ title: "An Anthropic API key", body: [h("p", { class: "small dim" }, "This server has no key of its own (ANTHROPIC_API_KEY). Grades are asked for with this one; it goes to this server with each request and is not stored there."), h("label", { class: "f" }, "Key", k), h("label", { class: "row nw small" }, keep, "Remember it in this browser")],
        foot: [h("button", { class: "btn", onclick: () => done(null) }, "Cancel"), h("button", { class: "btn primary", onclick: () => { const v = k.value.trim(); if (!v) return; if (keep.checked) store.set("judge.key", v); done({ key: v, model: cfg.agent?.model }); } }, "Grade")] });
      setTimeout(() => k.focus(), 30);
    });
  }
  /** Claude grades the pool of each query: the rows either ranking shows that it has not graded yet. */
  async function judge(qs) {
    const auth = await apiKey();
    if (!auth) return;
    judging = true; drawSet();
    let graded = 0, failed = 0, firstError = null;
    try {
      for (const q of qs) {
        const res = q === st.q && results.every((x) => x?.rows) ? results : await Promise.all(st.rankings.map((r) => query(r, q)));
        const have = llmFor(q);
        const keys = [...new Set(res.flatMap((x) => x.rows.map((r) => r.key)))].filter((k) => have[k] == null);
        if (!keys.length) continue;
        const j = await api("/v1/judge", { body: { table: st.table, query: q, keys, api_key: auth.key } });
        const got = {};
        for (const g of j.grades) { if (g.grade != null) { got[g.key] = g.grade; graded++; } else { failed++; firstError ||= g.error; } }
        addLlm(q, got);
      }
      toast(graded || !failed ? `Claude graded ${fmt.plural(graded, "row")}${failed ? `; ${failed} without a grade (${firstError})` : ""}` : `No grades: ${firstError}`, failed ? "warn" : "ok", failed ? 9000 : 4000);
    } catch (e) { toast(e.message, "bad", 9000); }
    judging = false; drawSet(); drawCompare();
  }

  // ------------------------------------------------------------ running
  const runSoon = debounce(() => run(), 400);
  async function run() {
    drawSet();
    if (!st.q.trim()) { results = [null, null]; drawCompare(); return; }
    const mine = ++running;
    const out = await Promise.all(st.rankings.map((r) => query(r, st.q).catch((e) => ({ error: e.message }))));
    if (mine !== running) return;
    results = out;
    drawCompare();
  }

  // ------------------------------------------------------------ the two lists
  function drawCompare() {
    const grades = measured();
    const cards = st.rankings.map((r, i) => rankingCard(r, i, results[i], grades));
    const shift = h("div", { class: "rl-shift" });
    clear(cmp, cards[0], shift, cards[1]);
    requestAnimationFrame(() => drawShift(shift, grades));
  }
  function rankingCard(r, i, res, grades) {
    const letter = "AB"[i];
    const ctl = h("div", { class: "rl-ctl" });
    const fsel = (key, list, title) => h("select", { class: "select sm", title, onchange: (e) => { r[key] = e.target.value; save(); run(); } }, list.map((f) => h("option", { value: f.name, selected: f.name === r[key] }, f.name)));
    if (["bm25", "fuzzy", "rrf", "weighted", "boost"].includes(r.preset)) ctl.append(fsel("field", textF(), "words in"));
    if (["vector", "rrf", "weighted"].includes(r.preset)) ctl.append(vecF().length ? fsel("vfield", vecF(), "meaning of") : h("span", { class: "small error" }, "no vector field"));
    if (r.preset === "boost") ctl.append(fsel("nfield", numF(), "boosted by"), slider(r, "w", 0, 1, 0.01, (v) => `× (1 + ${v} × ${r.nfield})`));
    if (r.preset === "weighted") ctl.append(
      h("select", { class: "select sm", title: "How each score is put on one scale: tmm divides by the best row's score from the least possible one (0 for words, -1 for meaning); minmax spans the rows found; zscore centres on their mean", onchange: (e) => { r.norm = e.target.value; save(); drawCompare(); run(); } }, NORMS.map(([v, l]) => h("option", { value: v, selected: v === (r.norm || "tmm") }, l))),
      slider(r, "alpha", 0, 1, 0.05, (v) => `${fmt.pct(v, 0)} words · ${fmt.pct(1 - v, 0)} meaning`));
    if (r.preset === "rrf") ctl.append(h("label", { class: "row nw small dim", title: "RRF's k: smaller trusts the top of each ranking more" }, "k", h("select", { class: "select sm", onchange: (e) => { r.k = Number(e.target.value); save(); run(); } }, RRF_KS.map((k) => h("option", { value: k, selected: k === (r.k ?? 60) }, k)))));
    if (r.preset === "profile") { const ps = tableOf(st.table)?.profiles || []; ctl.append(ps.length ? h("select", { class: "select sm", onchange: (e) => { r.profile = e.target.value; save(); run(); } }, ps.map((p) => h("option", { value: p.name, selected: p.name === r.profile }, p.name))) : h("span", { class: "small muted" }, `${st.table} has no rank profiles yet: save a ranking as one below.`)); if (!r.profile && ps[0]) r.profile = ps[0].name; }
    if (r.preset === "custom") { const ta = h("textarea", { class: "input mono sm", rows: 2, placeholder: "bm25(body, :q) * decay(year, 2026, 3)", value: r.custom, style: { width: "100%" }, oninput: (e) => { r.custom = e.target.value; save(); runSoon(); } }); ctl.append(ta); }
    const m = res?.rows ? measures(res.rows.map((x) => x.key), grades, st.k) : null;
    const other = results[1 - i]?.rows ? measures(results[1 - i].rows.map((x) => x.key), grades, st.k) : null;
    const tile = (label, v, ov, f = (x) => x == null ? "–" : x.toFixed(3), title) => h("div", { class: "rl-m", title }, h("span", { class: "lbl" }, label), h("b", {}, f(v)),
      i === 1 && v != null && ov != null && Math.abs(v - ov) > 1e-9 ? h("span", { class: "d " + (v > ov ? "up" : "down") }, (v > ov ? "▲ " : "▼ ") + Math.abs(v - ov).toFixed(3)) : null);
    const list = h("div", { class: "rl-list" });
    if (res?.error) list.append(h("div", { class: "error", style: { margin: "10px" } }, res.error));
    else if (!res) list.append(empty("search", "Type a query", "Both rankings run as you type."));
    else if (!res.rows.length) list.append(empty("search", "No rows", "Nothing matched."));
    else { const mine = gradesFor(), llm = llmFor(); res.rows.forEach((row, n) => list.append(rowEl(row, n, mine, llm, i))); }
    return h("section", { class: "card rl-card", style: { "--c": COLORS[i] } },
      h("div", { class: "rl-hd" }, h("span", { class: "rl-letter" }, letter),
        h("select", { class: "select sm", onchange: (e) => { r.preset = e.target.value; save(); drawCompare(); run(); } }, PRESETS.map(([v, l]) => h("option", { value: v, selected: v === r.preset }, l))),
        h("span", { class: "sep" }),
        res?.took != null ? h("span", { class: "small muted" }, fmt.ms(res.took)) : null,
        h("button", { class: "btn sm ghost", title: "Keep this ranking with the table as a rank profile", disabled: r.preset === "profile" || !expr(r), onclick: () => saveProfile(r) }, icon("plus"), "Profile")),
      ctl,
      h("div", { class: "rl-expr mono small", title: "ORDER BY" }, expr(r) || "write an expression"),
      h("div", { class: "rl-ms" },
        tile("nDCG@" + st.k, m?.ndcg, other?.ndcg, undefined, "Discounted cumulative gain of the grades, over the best order the grades allow"),
        tile("good in top " + st.k, m?.precision, other?.precision, (x) => x == null ? "–" : fmt.pct(x, 0), "Rows graded good or perfect"),
        tile("first good", m?.mrr, other?.mrr, (x) => x ? `#${Math.round(1 / x)}` : "–", "The rank of the first row graded good or perfect (reciprocal rank)"),
        h("div", { class: "rl-m", title: "Rows of the top that have a grade: measures count the others as bad" }, h("span", { class: "lbl" }, "graded"), h("b", {}, m ? `${m.judged}/${m.n}` : "–"))),
      list);
  }
  function slider(r, key, min, max, step, label) {
    const out = h("span", { class: "small dim mono" }, label(r[key]));
    return h("span", { class: "row nw", style: { gap: "6px" } }, h("input", { type: "range", min, max, step, value: r[key], style: { width: "120px" }, oninput: (e) => { r[key] = Number(e.target.value); out.textContent = label(r[key]); save(); runSoon(); } }), out);
  }
  function rowEl(row, n, grades, llm, side) {
    const g = grades[row.key], c = llm[row.key];
    const feats = Object.entries(row.feats).filter(([, v]) => typeof v === "number");
    const el = h("div", { class: "rl-row", dataset: { key: row.key }, style: { height: ROW_H + "px" } },
      h("span", { class: "rl-n" }, n + 1),
      h("div", { class: "rl-t" }, h("div", { class: "ellipsis" }, row.title == null || row.title === "" ? row.key : String(row.title)), h("div", { class: "small muted mono ellipsis" }, row.key, " · ", fmt.si(row.score, 3))),
      c != null ? h("span", { class: "rl-llm g" + c, title: `Claude's grade: ${GRADES[c][1]} (UMBRELA's prompt)${g != null && g !== c ? `; yours is ${GRADES[g][1]}` : ""}` }, "C", c) : null,
      h("div", { class: "rl-g", role: "group", "aria-label": "grade" }, GRADES.map(([v, l]) => h("button", { class: "g" + v, "aria-pressed": String(g === v), title: l, onclick: () => setGrade(row.key, g === v ? null : v) }, v))));
    if (feats.length) {
      el.addEventListener("pointerenter", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `#${n + 1} in ${"AB"[side]}: the values it was ranked by`), feats.map(([k, v]) => h("div", { class: "r" }, h("span", { class: "mono" }, k), h("b", { class: "n" }, fmt.si(v, 3))))]));
      el.addEventListener("pointerleave", hideTip);
    }
    return el;
  }
  function drawShift(box, grades) {
    const [a, b] = results;
    const W = box.clientWidth || 110;
    const la = cmp.querySelectorAll(".rl-card")[0]?.querySelector(".rl-list"), lb = cmp.querySelectorAll(".rl-card")[1]?.querySelector(".rl-list");
    if (!a?.rows || !b?.rows || !la || !lb) { clear(box); return; }
    const at = box.getBoundingClientRect().top;
    const topA = la.getBoundingClientRect().top - at, topB = lb.getBoundingClientRect().top - at;
    const n = Math.max(a.rows.length, b.rows.length);
    const H = Math.max(topA, topB) + n * ROW_H + 10;
    const svgEl = s("svg", { width: W, height: H, role: "img", "aria-label": "how each row moved from A to B" });
    const posB = new Map(b.rows.map((r, i) => [r.key, i]));
    const posA = new Map(a.rows.map((r, i) => [r.key, i]));
    const y = (i) => topA + i * ROW_H + ROW_H / 2, yb = (j) => topB + j * ROW_H + ROW_H / 2;
    let up = 0, down = 0, same = 0, gone = 0;
    a.rows.forEach((r, i) => {
      const j = posB.get(r.key);
      const cls = j == null ? "gone" : j < i ? "up" : j > i ? "down" : "same";
      if (j == null) gone++; else if (j < i) up++; else if (j > i) down++; else same++;
      const good = (grades[r.key] ?? -1) >= 2;
      const d = j == null ? `M0,${y(i)} C${W * 0.3},${y(i)} ${W * 0.35},${y(i)} ${W * 0.42},${y(i)}` : `M0,${y(i)} C${W * 0.5},${y(i)} ${W * 0.5},${yb(j)} ${W},${yb(j)}`;
      const p = s("path", { class: `sh ${cls}${good ? " good" : ""}`, d });
      p.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, r.title || r.key), h("div", {}, j == null ? `#${i + 1} in A, not in B's top ${st.k}` : j === i ? `#${i + 1} in both` : `#${i + 1} in A → #${j + 1} in B`)]));
      p.addEventListener("pointerleave", hideTip);
      svgEl.append(p);
      if (j == null) svgEl.append(s("circle", { class: "sh-end gone", cx: W * 0.42, cy: y(i), r: 3 }));
    });
    b.rows.forEach((r, j) => { if (!posA.has(r.key)) svgEl.append(s("path", { class: "sh new", d: `M${W * 0.58},${yb(j)} L${W},${yb(j)}` }), s("circle", { class: "sh-end new", cx: W * 0.58, cy: yb(j), r: 3 })); });
    const overlap = a.rows.filter((r) => posB.has(r.key)).length;
    clear(box, h("div", { class: "rl-shift-hd small" }, h("b", {}, `${overlap}/${a.rows.length}`), " shared", h("div", { class: "muted" }, `${up} up · ${down} down · ${same} same`)), svgEl);
  }

  // ------------------------------------------------------------ the set
  async function evaluate() {
    const qs = querySet();
    clear(evalBox, h("div", { class: "small muted", style: { padding: "12px" } }, `Running ${fmt.plural(qs.length * 2, "query", "queries")}…`));
    try {
      const per = [];
      for (const q of qs) {
        const g = measured(q);
        const [ra, rb] = await Promise.all(st.rankings.map((r) => query(r, q)));
        per.push({ q, a: measures(ra.rows.map((x) => x.key), g, st.k), b: measures(rb.rows.map((x) => x.key), g, st.k) });
      }
      const mean = (side, k) => { const xs = per.map((p) => p[side][k]).filter((x) => x != null); return xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : null; };
      evalOut = { per, mean };
      const tunable = st.rankings.map((r, i) => (r.preset === "weighted" || r.preset === "rrf") && r.field && r.vfield ? i : -1).filter((i) => i >= 0);
      clear(evalBox,
        h("section", { class: "card" }, h("div", { class: "hd" }, h("h3", {}, "Over the graded queries"), h("span", { class: "sub" }, `${fmt.plural(qs.length, "query", "queries")} · top ${st.k} · ${SOURCES.find(([v]) => v === source())[1]} grades · rows without a grade count as bad`),
            h("span", { class: "sep" }), h("button", { class: "btn sm", disabled: judging, title: "Claude grades the rows of either ranking it has not graded, for every query of the set", onclick: async () => { await judge(qs); evaluate(); } }, icon("agent"), "Grade the pools with Claude")),
          h("div", { class: "bd" }, h("div", { class: "grid g2" },
            h("div", {}, h("div", { class: "rl-sum" }, ["ndcg", "precision", "mrr"].map((k) => h("div", { class: "rl-m big" }, h("span", { class: "lbl" }, { ndcg: `mean nDCG@${st.k}`, precision: `good in top ${st.k}`, mrr: "mean reciprocal rank" }[k]),
              h("div", { class: "row nw", style: { gap: "10px" } }, ["a", "b"].map((side, i) => h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "rl-letter sm", style: { "--c": COLORS[i] } }, "AB"[i]), h("b", {}, mean(side, k) == null ? "–" : k === "precision" ? fmt.pct(mean(side, k), 0) : mean(side, k).toFixed(3)))))))),
              wins(per), agreeLine(qs)),
            h("div", {}, h("div", { class: "small dim", style: { marginBottom: "6px" } }, `nDCG@${st.k} of B minus A, a query a bar`), deltaChart(per))))),
        tunable.map((i) => h("section", { class: "card", style: { marginTop: "14px" } }, h("div", { class: "hd" },
          h("h3", {}, st.rankings[i].preset === "rrf" ? `Tune ${"AB"[i]}'s k` : `Tune ${"AB"[i]}'s weight`),
          h("span", { class: "sub" }, st.rankings[i].preset === "rrf" ? `mean nDCG over the set at each k (${qs.length < 20 ? "a few more graded queries make the choice firmer" : "fit on these queries"})` : `mean nDCG over the set at each share of words, ${st.rankings[i].norm || "tmm"} on both`)),
          h("div", { class: "bd" }, sweepBox(i, qs)))));
    } catch (e) { clear(evalBox, h("div", { class: "error", style: { margin: "12px" } }, e.message)); }
  }
  /** How Claude's grades agree with yours over the set. */
  function agreeLine(qs) {
    const mine = {}, llm = {};
    for (const q of qs) { for (const [k, v] of Object.entries(gradesFor(q))) mine[q + "\u0000" + k] = v; for (const [k, v] of Object.entries(llmFor(q))) llm[q + "\u0000" + k] = v; }
    const a = agreement(mine, llm);
    if (!a.n) return null;
    return h("div", { class: "small dim", style: { marginTop: "6px" }, title: "Over the rows both you and Claude graded. Kappa weights a disagreement by its size: 1 is full agreement, 0 what chance would give." },
      `Claude and you graded ${fmt.plural(a.n, "row")} alike: the same grade ${fmt.pct(a.exact, 0)}, within one ${fmt.pct(a.withinOne, 0)}, weighted kappa ${a.kappa == null ? "–" : a.kappa.toFixed(2)}.`);
  }
  function wins(per) {
    const w = per.filter((p) => (p.b.ndcg ?? 0) > (p.a.ndcg ?? 0) + 1e-9).length, l = per.filter((p) => (p.b.ndcg ?? 0) < (p.a.ndcg ?? 0) - 1e-9).length;
    return h("div", { class: "small dim", style: { marginTop: "10px" } }, h("b", {}, `B wins ${w}`), `, loses ${l}, ties ${per.length - w - l}.`);
  }
  function deltaChart(per) {
    const rows = per.map((p) => ({ q: p.q, d: (p.b.ndcg ?? 0) - (p.a.ndcg ?? 0) })).sort((a, b) => b.d - a.d);
    const max = Math.max(0.05, ...rows.map((r) => Math.abs(r.d)));
    return h("div", { class: "rl-delta" }, rows.map((r) => h("div", { class: "rl-dr", onclick: () => { st.q = r.q; save(); drawTop(); run(); }, title: "Open this query" },
      h("span", { class: "q ellipsis" }, r.q),
      h("span", { class: "bar" }, h("span", { class: "mid" }), h("span", { class: "b " + (r.d >= 0 ? "pos" : "neg"), style: { width: (Math.abs(r.d) / max * 50) + "%", [r.d >= 0 ? "left" : "right"]: "50%" } })),
      h("span", { class: "v mono" }, (r.d >= 0 ? "+" : "") + r.d.toFixed(3)))));
  }
  function sweepBox(idx, qs) {
    const r = st.rankings[idx];
    const rrf = r.preset === "rrf";
    // Each point: where it sits on the axis (0..1), its label, and the
    // setting it tries.
    const tries = rrf
      ? RRF_KS.map((k, i) => ({ x: i / (RRF_KS.length - 1), label: `k ${k}`, over: { k } }))
      : Array.from({ length: 11 }, (_, i) => ({ x: i / 10, label: i === 0 ? "meaning only" : i === 10 ? "words only" : `${i * 10}%`, over: { alpha: i / 10 } }));
    const box = h("div", {}, h("div", { class: "small muted" }, `Trying ${tries.length} ${rrf ? "values of k" : "weights"}…`));
    (async () => {
      const pts = [];
      for (const t of tries) {
        let sum = 0, n = 0;
        for (const q of qs) { const res = await query(r, q, t.over); const m = measures(res.rows.map((x) => x.key), measured(q), st.k); if (m.ndcg != null) { sum += m.ndcg; n++; } }
        pts.push({ ...t, v: n ? sum / n : 0 });
      }
      const best = pts.reduce((a, b) => (b.v > a.v ? b : a));
      const cur = rrf ? (RRF_KS.indexOf(r.k ?? 60) / (RRF_KS.length - 1)) : r.alpha;
      const said = rrf ? `Best: k ${best.over.k}, nDCG ${best.v.toFixed(3)}.` : `Best: ${fmt.pct(best.over.alpha, 0)} words, ${fmt.pct(1 - best.over.alpha, 0)} meaning, nDCG ${best.v.toFixed(3)}.`;
      clear(box, sweepChart(pts, best, cur, rrf), h("div", { class: "row", style: { marginTop: "8px" } }, h("span", { class: "small dim" }, said),
        h("button", { class: "btn sm primary", onclick: () => { Object.assign(r, best.over); save(); drawCompare(); run(); toast(rrf ? `${"AB"[idx]} uses k ${best.over.k}` : `${"AB"[idx]} uses ${fmt.pct(best.over.alpha, 0)} words`, "ok"); } }, "Use it")));
    })().catch((e) => clear(box, h("div", { class: "error" }, e.message)));
    return box;
  }
  function sweepChart(pts, best, cur, rrf) {
    const W = 640, H = 210, M = { l: 44, r: 44, t: 26, b: 30 };
    const lo = Math.min(...pts.map((p) => p.v)), hi = Math.max(...pts.map((p) => p.v));
    const y0 = Math.max(0, lo - (hi - lo) * 0.2 - 0.01), y1 = Math.min(1, hi + (hi - lo) * 0.2 + 0.01);
    const X = (a) => M.l + a * (W - M.l - M.r), Y = (v) => H - M.b - (v - y0) / (y1 - y0 || 1) * (H - M.t - M.b);
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "rl-sweep", role: "img", "aria-label": rrf ? "mean nDCG by RRF's k" : "mean nDCG by the share of words" });
    for (let k = 0; k <= 4; k++) { const v = y0 + (y1 - y0) * k / 4; svgEl.append(s("line", { class: "gl", x1: M.l, x2: W - M.r, y1: Y(v), y2: Y(v) }), s("text", { class: "ax", x: M.l - 6, y: Y(v) + 4, "text-anchor": "end" }, v.toFixed(2))); }
    pts.forEach((p, i) => { if (rrf || i % 2 === 0) svgEl.append(s("text", { class: "ax", x: X(p.x), y: H - 10, "text-anchor": "middle" }, p.label)); });
    svgEl.append(s("line", { class: "cur", x1: X(cur), x2: X(cur), y1: M.t, y2: H - M.b }));
    svgEl.append(s("path", { class: "ln", d: pts.map((p, i) => `${i ? "L" : "M"}${X(p.x)},${Y(p.v)}`).join("") }));
    for (const p of pts) {
      const c = s("circle", { class: "pt" + (p === best ? " best" : ""), cx: X(p.x), cy: Y(p.v), r: p === best ? 6 : 4 });
      c.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, rrf ? p.label : `${fmt.pct(p.over.alpha, 0)} words`), h("div", {}, `nDCG ${p.v.toFixed(3)}`)]));
      c.addEventListener("pointerleave", hideTip);
      svgEl.append(c);
    }
    svgEl.append(s("text", { class: "ax best", x: X(best.x), y: Y(best.v) - 12, "text-anchor": "middle" }, `best ${best.v.toFixed(3)}`));
    return svgEl;
  }

  // ------------------------------------------------------------ rank profiles
  function saveProfile(r) {
    const name = h("input", { class: "input", value: r.preset === "weighted" ? "hybrid" : r.preset, style: { width: "100%" } });
    const t = tableOf(st.table);
    const e = expr(r).replace(/:v\b/g, ":v").replace(/:q\b/g, ":q");
    const preview = h("div");
    const sqlFor = () => `CREATE OR REPLACE RANK PROFILE ${quoteName(name.value.trim() || "p")} ON ${quoteName(t.table)} AS\n  SELECT * FROM ${quoteName(t.table)} ORDER BY ${e} DESC;`;
    const draw = () => clear(preview, sqlBlock(sqlFor()), h("div", { class: "small muted", style: { marginTop: "6px" } }, `Queries then rank by it: ORDER BY rank_profile('${name.value.trim()}', q => :q${/:v\b/.test(e) ? ", v => :v" : ""}).`));
    name.oninput = draw; draw();
    const m = modal({ title: "Keep this ranking with the table", body: [h("label", { class: "f" }, "Name", name), preview],
      foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), h("button", { class: "btn primary", onclick: async () => {
        try { await api("/v1/sql", { body: { sql: sqlFor() } }); m.close(); toast(`Rank profile ${name.value.trim()} saved`, "ok"); invalidate(); ts = await tables(true); }
        catch (err) { toast(err.message, "bad", 9000); }
      } }, "Save the profile")] });
  }
}
