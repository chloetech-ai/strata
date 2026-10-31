// Semantic IDs: the tree a codebook cuts a table's vectors into. A
// sunburst shows the codes under a prefix two levels deep, sized by their
// rows and colored by what most of their rows are (a field chosen); a
// click zooms in. Beside it the codes are decoded one number at a time,
// as a generative recommender does with /v1/sid/next, under a filter that
// prunes the codes no matching row has; and the codebook's use per level.

import { h, s, icon, clear, fmt, api, store, empty, failure, sqlBlock, debounce, showTip, hideTip } from "../core.js";
import { tables, table as tableOf } from "../catalog.js";
import { SERIES } from "../charts.js";
import { quoteName } from "../sqledit.js";

export default { id: "sid", title: "Semantic IDs", icon: "sid", group: "Explore", full: true, keywords: "semantic ids codebook residual quantization rq-vae generative retrieval recommender beam search decoding prefix codes clusters sunburst", render };

const sidFields = (t) => (t?.short?.fields || []).filter((f) => f.semantic_id);
const depth = (c) => (c ? c.split(".").length : 0);
const lit = (v) => `'${String(v).replace(/'/g, "''")}'`;

function render(page) {
  let ts = [], st = store.get("sid.state", null), levels = 3, palette = new Map(), cache = new Map(), run = 0;
  const save = () => store.set("sid.state", st);
  const top = h("div", { class: "sd-top" });
  const crumbs = h("div", { class: "sd-crumbs" });
  const sun = h("div", { class: "card sd-sun" });
  const cols = h("div", { class: "card sd-cols" });
  const detail = h("div", { class: "sd-detail" });
  const book = h("div", { class: "card sd-book" });
  page.append(h("div", { class: "sd" }, top, crumbs, h("div", { class: "sd-grid" }, h("div", { class: "sd-left" }, sun, cols), h("div", { class: "sd-right" }, detail, book))));

  tables().then(async (list) => {
    ts = list.filter((t) => sidFields(t).length);
    if (!ts.length) { drawNone(); return; }
    if (!st || !tableOf(st.table) || !sidFields(tableOf(st.table)).some((f) => f.name === st.field)) st = fresh(ts[0]);
    save(); await start();
  }).catch((e) => clear(sun, failure(e, "The tables")));

  function fresh(t) {
    const f = sidFields(t)[0];
    const color = (t.short.fields || []).find((x) => x.type === "keyword" && !x.multi && x.name !== t.short.key && !x.semantic_id)?.name || "";
    const shown = (t.short.fields || []).find((x) => x.type === "text" && x.stored !== false)?.name || t.short.key;
    return { table: t.table, field: f.name, color, shown, where: "", focus: "", path: [] };
  }
  function drawNone() {
    clear(top);
    clear(sun, empty("sid", "No semantic-ID field", "A KEYWORD[] field declared as the semantic IDs of a VECTOR field, by a codebook in the models directory, holds every prefix of each row's code."),
      h("div", { class: "rx-howto" }, sqlBlock("CREATE TABLE items (\n  id KEYWORD PRIMARY KEY,\n  title TEXT,\n  emb VECTOR(64),\n  sid KEYWORD[] OPTIONS(semantic_id = 'emb', codebook = 'items-rq')\n);"),
        h("div", { class: "small muted" }, "Train the codebook with strata.train.codebook in Python (residual k-means, up to 4 levels of up to 256), or export an RQ-VAE's levels as safetensors.")));
    cols.remove(); book.remove();
  }

  async function start() {
    cache.clear();
    const t = tableOf(st.table);
    try {
      const j = await api("/v1/sql", { body: { sql: `SELECT ${quoteName(st.field)} FROM ${quoteName(st.table)} WHERE ${quoteName(st.field)} IS NOT NULL LIMIT 1` } });
      const code = [].concat(j.rows[0]?.[0] || []);
      levels = Math.max(1, code.length - 1);
    } catch { levels = 3; }
    palette = new Map();
    if (st.color) {
      try {
        const j = await api("/v1/sql", { body: { sql: `SELECT ${quoteName(st.color)}, COUNT(*) AS n FROM ${quoteName(st.table)} GROUP BY ${quoteName(st.color)} ORDER BY n DESC LIMIT 7` } });
        j.rows.forEach(([v], i) => palette.set(String(v), SERIES[i]));
      } catch { /* no colors */ }
    }
    drawTop(); go(st.focus || "");
    drawBook();
    void t;
  }

  // ------------------------------------------------------------ data
  /** The next codes of each prefix, with their rows under the filter: Map(prefix → [{code, count}]). */
  async function next(prefixes) {
    const want = prefixes.filter((p) => !cache.has(p));
    for (let i = 0; i < want.length; i += 64) {
      const j = await api("/v1/sid/next", { body: { table: st.table, field: st.field, prefixes: want.slice(i, i + 64), ...(st.where.trim() ? { where: st.where } : {}) } });
      for (const x of j.next) cache.set(x.prefix, x.next);
    }
    return new Map(prefixes.map((p) => [p, cache.get(p) || []]));
  }
  /** What most rows under each code at a depth are, by the color field: Map(code → {top, share, mix}). */
  async function colorsAt(prefix, d) {
    if (!st.color || d > levels) return new Map();
    const where = [prefix ? `${quoteName(st.field)} = ${lit(prefix)}` : null, st.where.trim() ? `(${st.where})` : null].filter(Boolean);
    const sql = `SELECT sid_at(${quoteName(st.field)}, ${d}) AS code, ${quoteName(st.color)} AS v, COUNT(*) AS n FROM ${quoteName(st.table)}${where.length ? " WHERE " + where.join(" AND ") : ""} GROUP BY code, v LIMIT 10000`;
    try {
      const j = await api("/v1/sql", { body: { sql } });
      const by = new Map();
      for (const [code, v, n] of j.rows) { const m = by.get(code) || new Map(); m.set(String(v), n); by.set(code, m); }
      return new Map([...by].map(([code, m]) => {
        const tot = [...m.values()].reduce((a, b) => a + b, 0);
        const [topV, topN] = [...m.entries()].sort((a, b) => b[1] - a[1])[0];
        return [code, { top: topV, share: topN / tot, mix: [...m.entries()].sort((a, b) => b[1] - a[1]) }];
      }));
    } catch { return new Map(); }
  }
  const colorOf = (v) => palette.get(String(v)) || "var(--muted)";

  // ------------------------------------------------------------ controls
  function drawTop() {
    const t = tableOf(st.table);
    const kws = (t.short.fields || []).filter((x) => x.type === "keyword" && !x.multi && x.name !== t.short.key && !x.semantic_id);
    clear(top,
      h("label", { class: "rx-lbl" }, "table", h("select", { class: "select sm", onchange: (e) => { st = fresh(tableOf(e.target.value)); save(); start(); } }, ts.map((x) => h("option", { value: x.table, selected: x.table === st.table }, x.table)))),
      h("label", { class: "rx-lbl" }, "codes", h("select", { class: "select sm mono", onchange: (e) => { st.field = e.target.value; st.focus = ""; save(); start(); } }, sidFields(t).map((f) => h("option", { value: f.name, selected: f.name === st.field }, `${f.name} (${f.semantic_id.codebook} of ${f.semantic_id.from})`)))),
      h("label", { class: "rx-lbl" }, "color by", h("select", { class: "select sm mono", onchange: (e) => { st.color = e.target.value; save(); start(); } }, h("option", { value: "" }, "(none)"), kws.map((f) => h("option", { value: f.name, selected: f.name === st.color }, f.name)))),
      h("span", { class: "rl-qwrap sd-where" }, icon("filter"), h("input", { class: "input rl-q mono", style: { fontSize: "13px" }, value: st.where, placeholder: "Only codes with rows where… in_stock = TRUE AND price < 50", oninput: debounce((e) => { st.where = e.target.value; save(); cache.clear(); go(st.focus); drawBook(); }, 500) })),
      h("span", { class: "small muted" }, `${levels} levels, then a tie`));
  }
  function drawCrumbs() {
    const parts = st.focus ? st.focus.split(".") : [];
    clear(crumbs, h("button", { class: "chip", "aria-pressed": String(!parts.length), onclick: () => go("") }, icon("layers"), "all codes"),
      parts.map((p, i) => { const c = parts.slice(0, i + 1).join("."); return [h("span", { class: "muted" }, "›"), h("button", { class: "chip mono", "aria-pressed": String(i === parts.length - 1), onclick: () => go(c) }, c)]; }));
  }

  // ------------------------------------------------------------ zoom
  async function go(focus) {
    const mine = ++run;
    st.focus = focus; save(); drawCrumbs();
    try {
      const d = depth(focus);
      const kids = (await next([focus])).get(focus);
      const grand = d + 1 < levels + 1 ? await next(kids.map((k) => k.code)) : new Map();
      const [c1, c2] = await Promise.all([colorsAt(focus, d + 1), colorsAt(focus, d + 2)]);
      if (mine !== run) return;
      drawSun(focus, kids, grand, c1, c2);
      drawCols(focus);
      drawDetail(focus);
    } catch (e) { if (mine === run) clear(sun, failure(e, "The codes")); }
  }

  function drawSun(focus, kids, grand, c1, c2) {
    hideTip();
    const W = 520, R = 250, r0 = 70, r1 = 165, r2 = 245, cx = W / 2, cy = W / 2;
    const total = kids.reduce((a, k) => a + k.count, 0);
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${W}`, class: "sd-svg", role: "img", "aria-label": "codes under " + (focus || "the root") });
    const arc = (a0, a1, ri, ro) => {
      const gap = Math.min(0.006, (a1 - a0) / 4);
      a0 += gap; a1 -= gap;
      const p = (a, r) => `${(cx + r * Math.sin(a)).toFixed(2)},${(cy - r * Math.cos(a)).toFixed(2)}`;
      const large = a1 - a0 > Math.PI ? 1 : 0;
      return `M${p(a0, ro)}A${ro},${ro} 0 ${large} 1 ${p(a1, ro)}L${p(a1, ri)}A${ri},${ri} 0 ${large} 0 ${p(a0, ri)}Z`;
    };
    const tip = (e, code, n, c) => showTip(e.clientX, e.clientY, [h("div", { class: "t mono" }, code),
      h("div", { class: "r" }, h("b", {}, fmt.n(n)), h("span", { class: "n" }, `rows · ${fmt.pct(total ? n / total : 0)} of ${focus || "all"}`)),
      ...(c ? c.mix.slice(0, 4).map(([v, m]) => h("div", { class: "r" }, h("span", { class: "k box", style: { background: colorOf(v) } }), h("b", {}, fmt.pct(m / n)), h("span", { class: "n" }, v))) : [])]);
    let a = 0;
    for (const k of kids) {
      const span = total ? k.count / total * Math.PI * 2 : 0;
      const c = c1.get(k.code);
      const fill = c ? colorOf(c.top) : SERIES[0];
      const g = s("g", { class: "sd-sec", tabindex: 0, role: "button", "aria-label": `${k.code}: ${k.count} rows` });
      g.append(s("path", { d: arc(a, a + span, r0, r1), style: { fill, opacity: c ? 0.35 + 0.65 * c.share : 0.85 } }));
      // its children, in the outer ring
      let b = a;
      const gk = grand.get(k.code) || [];
      const gt = gk.reduce((x, y) => x + y.count, 0);
      for (const q of gk) {
        const sp = gt ? q.count / gt * span : 0;
        const cc = c2.get(q.code);
        const p = s("path", { d: arc(b, b + sp, r1 + 3, r2), class: "sd-out", style: { fill: cc ? colorOf(cc.top) : fill, opacity: cc ? 0.25 + 0.6 * cc.share : 0.5 } });
        p.addEventListener("pointermove", (e) => { e.stopPropagation(); tip(e, q.code, q.count, cc); });
        p.addEventListener("click", (e) => { e.stopPropagation(); go(depth(q.code) <= levels ? q.code : k.code); });
        g.append(p);
        b += sp;
      }
      if (span > 0.16) {
        const mid = a + span / 2, rr = (r0 + r1) / 2;
        g.append(s("text", { x: cx + rr * Math.sin(mid), y: cy - rr * Math.cos(mid) + 4, "text-anchor": "middle", class: "sd-lbl" }, k.code.split(".").pop()));
      }
      g.addEventListener("pointermove", (e) => tip(e, k.code, k.count, c));
      g.addEventListener("pointerleave", hideTip);
      g.addEventListener("click", () => go(k.code));
      g.addEventListener("keydown", (e) => { if (e.key === "Enter") go(k.code); });
      svgEl.append(g);
      a += span;
    }
    const up = focus ? focus.split(".").slice(0, -1).join(".") : null;
    const center = s("g", { class: "sd-center" + (up != null ? " up" : ""), role: up != null ? "button" : null, tabindex: up != null ? 0 : null },
      s("circle", { cx, cy, r: r0 - 4 }),
      s("text", { x: cx, y: cy - 6, "text-anchor": "middle", class: "sd-c1" }, focus || "all"),
      s("text", { x: cx, y: cy + 14, "text-anchor": "middle", class: "sd-c2" }, fmt.si(total) + " rows"),
      up != null ? s("text", { x: cx, y: cy + 30, "text-anchor": "middle", class: "sd-c3" }, "↑ up") : null);
    if (up != null) { center.addEventListener("click", () => go(up)); center.addEventListener("keydown", (e) => { if (e.key === "Enter") go(up); }); }
    svgEl.append(center);
    void R;
    const legend = st.color ? h("div", { class: "legend sd-legend" }, [...palette.entries()].map(([v, c]) => h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "k box", style: { background: c, width: "10px", height: "10px", borderRadius: "3px", display: "inline-block" } }), v)), h("span", { class: "muted" }, "· paler: mixed")) : null;
    clear(sun, h("div", { class: "hd" }, h("h3", {}, focus ? `Under ${focus}` : "Every code"), h("span", { class: "sub" }, `inner ring: level ${depth(focus) + 1}${depth(focus) + 2 <= levels + 1 ? `, outer: level ${depth(focus) + 2}${depth(focus) + 2 === levels + 1 ? " (ties: rows that share every level)" : ""}` : ""}; sized by rows${st.where.trim() ? " that pass the filter" : ""}; click to zoom`)),
      h("div", { class: "bd sd-sunbd" }, kids.length ? svgEl : empty("filter", "No codes", "No row under this prefix passes the filter."), legend));
  }

  // ------------------------------------------------------------ decoding
  async function drawCols(focus) {
    const parts = focus ? focus.split(".") : [];
    const prefixes = [""].concat(parts.map((_, i) => parts.slice(0, i + 1).join("."))).filter((p) => depth(p) <= levels);
    const m = await next(prefixes);
    const colEls = prefixes.map((p, i) => {
      const list = m.get(p) || [];
      const chosen = parts[i] != null ? parts.slice(0, i + 1).join(".") : null;
      const max = Math.max(1, ...list.map((x) => x.count));
      return h("div", { class: "sd-col" },
        h("div", { class: "sd-colh small muted" }, i < levels ? `level ${i + 1}` : "tie", h("span", {}, fmt.plural(list.length, "code"))),
        h("div", { class: "sd-colb" }, list.map((x) => h("button", { class: "sd-code" + (x.code === chosen ? " on" : ""), onclick: () => go(x.code) },
          h("span", { class: "mono" }, x.code.split(".").pop()),
          h("span", { class: "sd-cbar" }, h("i", { style: { width: (x.count / max * 100) + "%" } })),
          h("span", { class: "num small dim" }, fmt.si(x.count))))));
    });
    clear(cols, h("div", { class: "hd" }, h("h3", {}, "Decoding, one number at a time"), h("span", { class: "sub" }, "what /v1/sid/next answers a generative recommender at each step: only the numbers that still lead to a row passing the filter")),
      h("div", { class: "bd sd-colwrap" }, colEls));
    const wrap = cols.querySelector(".sd-colwrap"); if (wrap) wrap.scrollLeft = wrap.scrollWidth;
  }

  // ------------------------------------------------------------ one code
  async function drawDetail(code) {
    const t = tableOf(st.table);
    const where = [code ? `${quoteName(st.field)} = ${lit(code)}` : null, st.where.trim() ? `(${st.where})` : null].filter(Boolean);
    const cols = [t.short.key, st.shown !== t.short.key ? st.shown : null, st.color || null].filter(Boolean).map(quoteName);
    const sql = `SELECT ${cols.join(", ")}, ${quoteName(st.field)} FROM ${quoteName(st.table)}${where.length ? " WHERE " + where.join(" AND ") : ""} LIMIT 12`;
    let rows = [], n = null, mix = [];
    try {
      const [j, c] = await Promise.all([
        api("/v1/sql", { body: { sql } }),
        api("/v1/sql", { body: { sql: `SELECT COUNT(*) FROM ${quoteName(st.table)}${where.length ? " WHERE " + where.join(" AND ") : ""}` } }),
      ]);
      rows = j.rows; n = c.rows[0][0];
      if (st.color) mix = (await api("/v1/sql", { body: { sql: `SELECT ${quoteName(st.color)}, COUNT(*) AS n FROM ${quoteName(st.table)}${where.length ? " WHERE " + where.join(" AND ") : ""} GROUP BY ${quoteName(st.color)} ORDER BY n DESC LIMIT 8` } })).rows;
    } catch (e) { clear(detail, h("div", { class: "card" }, failure(e, "The rows"))); return; }
    const total = mix.reduce((a, [, x]) => a + x, 0);
    clear(detail, h("div", { class: "card" },
      h("div", { class: "hd" }, h("h3", {}, code ? h("span", { class: "mono" }, code) : "All rows"), h("span", { class: "sub" }, `${fmt.plural(n, "row")}${code ? ` share the start ${code}` : ""}`),
        h("div", { class: "tools" }, h("a", { class: "btn sm ghost", href: "#/sql?q=" + encodeURIComponent(sql.replace(/ LIMIT 12$/, " LIMIT 100")), title: "Open it in the SQL workbench" }, icon("play"), "Workbench"))),
      h("div", { class: "bd stack" },
        mix.length ? h("div", { class: "sd-mix", title: `${st.color} of the rows under it` }, mix.map(([v, x]) => h("i", { style: { flex: String(x), background: colorOf(v) }, title: `${v}: ${fmt.n(x)} (${fmt.pct(x / total)})` }))) : null,
        mix.length ? h("div", { class: "row wrap small dim", style: { gap: "4px 12px" } }, mix.slice(0, 5).map(([v, x]) => h("span", {}, h("span", { class: "rx-dot", style: { background: colorOf(v) } }), `${v} ${fmt.pct(x / total)}`))) : null,
        h("div", { class: "sd-rows" }, rows.map((r) => {
          const code = [].concat(r[r.length - 1] || []);
          return h("div", { class: "sd-row" }, h("div", { class: "rl-t" }, h("div", { class: "ellipsis" }, r[1] ?? r[0]), h("div", { class: "small muted mono" }, r[0], st.color ? ` · ${r[cols.length - 1]}` : "")), h("code", { class: "small sd-full", title: "the row's whole code" }, code[code.length - 1] || ""));
        })))));
  }

  // ------------------------------------------------------------ codebook
  async function drawBook() {
    const t = tableOf(st.table);
    const f = sidFields(t).find((x) => x.name === st.field);
    clear(book, h("div", { class: "hd" }, h("h3", {}, "The codebook"), h("span", { class: "sub mono" }, `${f.semantic_id.codebook} · of ${f.semantic_id.from}`)), h("div", { class: "bd" }, h("div", { class: "rx-wait" }, h("span", { class: "spin" }), "Counting codes per level…")));
    const where = st.where.trim() ? ` WHERE ${st.where}` : "";
    const per = [];
    for (let d = 1; d <= levels + 1; d++) {
      try {
        const j = await api("/v1/sql", { body: { sql: `SELECT sid_at(${quoteName(st.field)}, ${d}) AS code, COUNT(*) AS n FROM ${quoteName(st.table)}${where} GROUP BY code LIMIT 100000` } });
        const ns = j.rows.map((r) => r[1]).sort((a, b) => b - a);
        const rows = ns.reduce((a, b) => a + b, 0);
        per.push({ d, codes: ns.length, max: ns[0] || 0, mean: ns.length ? rows / ns.length : 0, rows, shared: d === levels ? ns.filter((x) => x > 1).reduce((a, b) => a + b, 0) : null });
      } catch { break; }
    }
    const maxCodes = Math.max(1, ...per.map((p) => p.codes));
    clear(book, h("div", { class: "hd" }, h("h3", {}, "The codebook"), h("span", { class: "sub mono" }, `${f.semantic_id.codebook} · of ${f.semantic_id.from}`)),
      h("div", { class: "bd flush" }, h("table", { class: "t compact" },
        h("thead", {}, h("tr", {}, ["level", "codes used", "", "rows per code", "largest"].map((c, i) => h("th", { class: i >= 3 ? "num" : "" }, c)))),
        h("tbody", {}, per.map((p) => h("tr", {},
          h("td", {}, p.d <= levels ? `${p.d}` : "full code"),
          h("td", { class: "num" }, fmt.n(p.codes)),
          h("td", { style: { width: "34%" } }, h("div", { class: "sd-cbar" }, h("i", { style: { width: (p.codes / maxCodes * 100) + "%" } }))),
          h("td", { class: "num" }, fmt.n(p.mean, 1)),
          h("td", { class: "num" }, fmt.n(p.max)))))),
        per.find((p) => p.shared != null) ? h("div", { class: "small muted", style: { padding: "8px 12px" } }, `${fmt.n(per.find((p) => p.shared != null).shared)} rows share all ${levels} levels with another row and are told apart by the tie: the more there are, the coarser the codebook is for this table.`) : null));
  }

  return () => { run++; };
}
