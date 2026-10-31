// Media search: images (or sounds) found by what they show. A text is
// embedded by the model's text tower and a dropped file by its image or
// audio tower, into one space, and the rows come back by similarity;
// words over a caption can be mixed in, facets and colour names filter,
// and any result finds more like it. A map lays every row's vector out
// in two dimensions (their two main directions), with the query and its
// hits marked on it. Files are read through /v1/media by their hash.

import { h, s, icon, clear, fmt, api, store, empty, seg, failure, drawer, sqlBlock, debounce, authHeader, showTip, hideTip, toast, copy } from "../core.js";
import { tables, table as tableOf } from "../catalog.js";
import { SERIES } from "../charts.js";
import { quoteName } from "../sqledit.js";

export default { id: "media", title: "Media search", icon: "media", group: "Explore", full: true, keywords: "media images photos pictures audio sound search by text image similarity siglip clap more like this embedding map visual search", render };

const COLORS = [["red", "#d64545"], ["orange", "#e8853d"], ["yellow", "#e6c62e"], ["green", "#3f9d55"], ["blue", "#3d7fd6"], ["purple", "#8a5cc7"], ["pink", "#e07aa8"], ["brown", "#8b5e3c"], ["black", "#1d1d1f"], ["white", "#f4f4f4"], ["gray", "#8d8f94"]];
const lit = (v) => `'${String(v).replace(/'/g, "''")}'`;

/** The tables with a MEDIA field and a vector embedded from it. */
function mediaTables(ts) {
  return ts.map((t) => {
    const fs = t.short?.fields || [];
    const media = fs.filter((f) => f.type === "media");
    const vec = fs.find((f) => f.type === "vector" && f.embed && media.some((m) => [].concat(f.embed.from).includes(m.name)));
    if (!vec) return null;
    const m = media.find((x) => [].concat(vec.embed.from).includes(x.name));
    return { t, media: m, vec, kind: m.kind || "image", text: fs.find((f) => f.type === "text" && f.indexed !== false && !f.derive), colors: fs.find((f) => f.type === "text" && f.derive && f.derive.model === "colors"), facets: fs.filter((f) => f.type === "keyword" && !f.multi && f.name !== t.short.key && f.type !== "media") };
  }).filter(Boolean);
}

const blobs = new Map();
/** An object URL of a file by its hash, fetched with the session's key (img tags send none); at most six at once. */
let inflight = 0; const waiting = [];
function fileUrl(sha) {
  if (blobs.has(sha)) return blobs.get(sha);
  const p = new Promise((resolve) => {
    const go = () => {
      inflight++;
      fetch("/v1/media/" + sha, { headers: authHeader() }).then((r) => r.ok ? r.blob() : null).then((b) => resolve(b ? URL.createObjectURL(b) : null)).catch(() => resolve(null))
        .finally(() => { inflight--; waiting.shift()?.(); });
    };
    if (inflight < 6) go(); else waiting.push(go);
  });
  blobs.set(sha, p);
  return p;
}
const parseMedia = (v) => { try { return typeof v === "string" ? JSON.parse(v) : v; } catch { return null; } };

function render(page) {
  let mts = [], cfg = null, st = store.get("media.state", null), res = null, mapData = null, run = 0, queryImage = null;
  const save = () => store.set("media.state", st);
  const top = h("div", { class: "md-top" });
  const filters = h("div", { class: "md-filters" });
  const info = h("div", { class: "md-info small muted" });
  const body = h("div", { class: "md-body" });
  page.append(h("div", { class: "md" }, top, filters, info, body));

  tables().then((ts) => {
    mts = mediaTables(ts);
    if (!mts.length) {
      clear(body, empty("media", "No media to search", "A MEDIA field with a VECTOR embedded from it by a model of images and text (siglip-base-patch16-224) or of sound (clap-htsat-unfused) makes a table searchable by what its files show."),
        h("div", { class: "rx-howto" }, sqlBlock("CREATE TABLE photos (\n  id KEYWORD PRIMARY KEY,\n  image MEDIA OPTIONS(kind = 'image'),\n  caption TEXT,\n  emb VECTOR OPTIONS(embed = 'image', model = 'siglip-base-patch16-224')\n);")));
      return;
    }
    if (!st || !mts.some((m) => m.t.table === st.table)) st = { table: mts[0].t.table, q: "", words: 0, facet: {}, color: "", mode: "grid", k: 40 };
    cfg = mts.find((m) => m.t.table === st.table);
    save(); drawTop(); drawFilters(); search();
  }).catch((e) => clear(body, failure(e, "The tables")));

  // ------------------------------------------------------------ the query
  function drawTop() {
    const qIn = h("input", { class: "input rl-q", value: st.q, placeholder: cfg.kind === "audio" ? "Describe a sound: rain on a tin roof" : "Describe a picture: a dog running on a beach", oninput: debounce((e) => { st.q = e.target.value; queryImage = null; save(); search(); }, 350) });
    const drop = h("label", { class: "md-drop", title: `Drop ${cfg.kind === "audio" ? "a sound file" : "an image"} here, or pick one, to find ${cfg.kind === "audio" ? "sounds" : "pictures"} like it` },
      icon(cfg.kind === "audio" ? "media" : "media"), h("span", {}, queryImage ? "Change the file" : `Search by ${cfg.kind === "audio" ? "a sound" : "an image"}`),
      h("input", { type: "file", accept: cfg.kind === "audio" ? "audio/*" : "image/*", style: { display: "none" }, onchange: (e) => e.target.files[0] && useFile(e.target.files[0]) }));
    for (const el of [drop, page]) {
      el.ondragover = (e) => { if ([...(e.dataTransfer?.items || [])].some((i) => i.kind === "file")) { e.preventDefault(); drop.classList.add("over"); } };
      el.ondragleave = () => drop.classList.remove("over");
      el.ondrop = (e) => { const f = e.dataTransfer?.files?.[0]; if (f) { e.preventDefault(); drop.classList.remove("over"); useFile(f); } };
    }
    clear(top,
      mts.length > 1 ? h("select", { class: "select", onchange: (e) => { st = { ...st, table: e.target.value, facet: {}, color: "" }; cfg = mts.find((m) => m.t.table === st.table); save(); drawTop(); drawFilters(); search(); } }, mts.map((m) => h("option", { value: m.t.table, selected: m.t.table === st.table }, m.t.table))) : null,
      queryImage ? h("span", { class: "md-qimg" }, queryImage.url ? h("img", { src: queryImage.url, alt: "" }) : icon("media"), h("span", { class: "small" }, queryImage.name), h("button", { class: "btn xs ghost", "aria-label": "Clear", onclick: () => { queryImage = null; drawTop(); search(); } }, icon("x")))
        : h("span", { class: "rl-qwrap" }, icon("search"), qIn),
      drop,
      cfg.text ? h("label", { class: "rx-lbl", title: `Mix BM25 over ${cfg.text.name} into the similarity: a weighted sum of the two, each scaled to 0..1` }, `words (${cfg.text.name})`,
        h("input", { type: "range", min: 0, max: 1, step: 0.05, value: st.words, style: { width: "110px" }, oninput: debounce((e) => { st.words = Number(e.target.value); save(); search(); }, 200) }), h("b", { class: "num" }, fmt.pct(st.words, 0))) : null,
      seg([["grid", "Grid"], ["map", "Map"]], st.mode, (v) => { st.mode = v; save(); draw(); }));
    if (!queryImage && !st.q) setTimeout(() => qIn.focus(), 30);
  }

  async function useFile(f) {
    if (f.size > 20 * 1024 * 1024) { toast("That file is over 20 MB", "warn"); return; }
    const b64 = await new Promise((ok) => { const r = new FileReader(); r.onload = () => ok(String(r.result).split(",")[1]); r.readAsDataURL(f); });
    queryImage = { b64, name: f.name, url: f.type.startsWith("image/") ? URL.createObjectURL(f) : null, kind: f.type.startsWith("audio/") ? "audio" : "image" };
    drawTop(); search();
  }

  async function drawFilters() {
    const t = cfg.t;
    const where = whereParts().filter((w) => !w.startsWith("__"));
    const chips = await Promise.all(cfg.facets.slice(0, 2).map(async (f) => {
      try {
        const j = await api("/v1/sql", { body: { sql: `SELECT ${quoteName(f.name)}, COUNT(*) AS n FROM ${quoteName(t.table)} GROUP BY ${quoteName(f.name)} ORDER BY n DESC LIMIT 14` } });
        return h("div", { class: "md-facet" }, h("span", { class: "small muted" }, f.name), j.rows.map(([v, n]) => h("button", { class: "chip", "aria-pressed": String(st.facet[f.name] === v), onclick: () => { st.facet[f.name] = st.facet[f.name] === v ? undefined : v; save(); drawFilters(); search(); } }, String(v), h("span", { class: "muted" }, fmt.si(n)))));
      } catch { return null; }
    }));
    void where;
    clear(filters, chips,
      cfg.colors ? h("div", { class: "md-facet" }, h("span", { class: "small muted", title: `the colours the ${cfg.colors.derive.model} model named in each image (${cfg.colors.name})` }, "colour"),
        COLORS.map(([c, hex]) => h("button", { class: "md-sw" + (st.color === c ? " on" : ""), style: { background: hex }, title: c, "aria-label": c, "aria-pressed": String(st.color === c), onclick: () => { st.color = st.color === c ? "" : c; save(); drawFilters(); search(); } }))) : null);
  }

  function whereParts() {
    const w = Object.entries(st.facet || {}).filter(([, v]) => v != null).map(([f, v]) => `${quoteName(f)} = ${lit(v)}`);
    if (st.color && cfg.colors) w.push(`MATCH(${quoteName(cfg.colors.name)}, ${lit(st.color)})`);
    return w;
  }

  /** The SQL and parameters of the search. */
  function sqlOf(like) {
    const t = cfg.t, V = quoteName(cfg.vec.name);
    const cols = [t.short.key, cfg.media.name, cfg.text?.name, cfg.colors?.name, ...cfg.facets.slice(0, 2).map((f) => f.name)].filter(Boolean);
    const where = whereParts();
    const params = {};
    let order;
    if (like) { params.v = { vector_of: like, field: cfg.vec.name, table: cfg.t.table }; order = `similarity(${V}, :v)`; }
    else if (queryImage) { params.v = { embed: { [queryImage.kind]: queryImage.b64 }, field: cfg.vec.name, table: cfg.t.table }; order = `similarity(${V}, :v)`; }
    else if (st.q.trim()) {
      params.v = { embed: st.q, field: cfg.vec.name, table: cfg.t.table };
      if (cfg.text && st.words > 0) { params.q = st.q; order = `${fmt.n(1 - st.words, 2)} * minmax(similarity(${V}, :v)) + ${fmt.n(st.words, 2)} * minmax(bm25(${quoteName(cfg.text.name)}, :q))`; }
      else order = `similarity(${V}, :v)`;
    }
    const sql = `SELECT ${[...new Set(cols)].map(quoteName).join(", ")}${order ? ", score()" : ""} FROM ${quoteName(t.table)}${where.length ? " WHERE " + where.join(" AND ") : ""}${order ? ` ORDER BY ${order} DESC` : ""} LIMIT ${st.k}`;
    return { sql, params, cols: [...new Set(cols)], ranked: !!order };
  }

  async function search(like) {
    const mine = ++run;
    const q = sqlOf(like);
    clear(info, h("span", { class: "spin" }), " searching…");
    try {
      const t0 = performance.now();
      const j = await api("/v1/query", { body: { sql: q.sql, params: q.params } });
      if (mine !== run) return;
      const idx = (c) => j.columns.indexOf(c);
      res = { q, like, took: j.took_ms, rtt: performance.now() - t0, rows: j.rows.map((r) => ({ key: String(r[idx(cfg.t.short.key)]), media: parseMedia(r[idx(cfg.media.name)]), text: cfg.text ? r[idx(cfg.text.name)] : null, colors: cfg.colors ? r[idx(cfg.colors.name)] : null, facets: Object.fromEntries(cfg.facets.slice(0, 2).map((f) => [f.name, r[idx(f.name)]])), score: q.ranked ? r[idx("score()")] : null })) };
      clear(info,
        h("span", {}, like ? `More like ${like}: ` : queryImage ? `Like ${queryImage.name}: ` : st.q.trim() ? `“${st.q}”: ` : "No query yet; the first rows: "),
        `${fmt.plural(res.rows.length, "row")} in ${fmt.ms(j.took_ms)} on the server, the ${queryImage ? cfg.kind : "text"}'s embedding included (${fmt.ms(res.rtt)} round trip)`,
        h("span", { class: "sep" }),
        h("button", { class: "btn xs ghost", onclick: () => drawer({ title: "The query", body: h("div", { class: "stack" }, sqlBlock(q.sql), h("pre", { class: "code small" }, JSON.stringify(q.params, (k, v) => k === "image" || k === "audio" ? `<${String(v).length} base64 characters>` : v, 2))) }) }, icon("sql"), "SQL"),
        h("a", { class: "btn xs ghost", href: "#/sql?q=" + encodeURIComponent(q.sql) + (q.params.v && !q.params.v.embed?.image ? "&p=" + encodeURIComponent(JSON.stringify(q.params)) : "") }, icon("play"), "Workbench"));
      draw();
    } catch (e) { if (mine === run) { clear(info); clear(body, failure(e, "The search")); } }
  }

  // ------------------------------------------------------------ results
  function draw() {
    if (!res) return;
    if (st.mode === "map") { drawMap(); return; }
    if (!res.rows.length) { clear(body, empty("filter", "Nothing matches", "Loosen the filters.")); return; }
    const max = Math.max(...res.rows.map((r) => r.score ?? 0)), min = Math.min(...res.rows.map((r) => r.score ?? 0));
    clear(body, h("div", { class: "md-grid" }, res.rows.map((r, i) => tile(r, i, max, min))));
  }
  function tile(r, i, max, min) {
    const img = h("img", { alt: r.text || r.key, loading: "lazy" });
    const ratio = r.media?.width && r.media?.height ? r.media.width / r.media.height : 1.4;
    const el = h("figure", { class: "md-tile", style: { flexGrow: String(ratio * 100), flexBasis: (ratio * 190) + "px" }, tabIndex: 0, onclick: () => open(r), onkeydown: (e) => { if (e.key === "Enter") open(r); } },
      h("div", { class: "md-img", style: { paddingBottom: (100 / ratio) + "%" } }, img),
      r.score != null ? h("span", { class: "md-rank" }, i + 1) : null,
      r.score != null ? h("span", { class: "md-score", title: "score(): the similarity (or the weighted sum)" }, h("i", { style: { width: ((r.score - min) / ((max - min) || 1) * 100) + "%" } }), fmt.n(r.score, 3)) : null,
      h("figcaption", {}, r.text || r.key));
    if (r.media?.sha256) fileUrl(r.media.sha256).then((u) => { if (u) { img.src = u; img.onload = () => el.classList.add("ld"); } });
    return el;
  }
  function open(r) {
    const img = h("img", { alt: r.text || r.key });
    if (r.media?.sha256) fileUrl(r.media.sha256).then((u) => { if (u) img.src = u; });
    const d = drawer({
      title: r.key, sub: r.media ? `${r.media.mime} · ${r.media.width || "?"}×${r.media.height || "?"} · ${fmt.bytes(r.media.bytes)}` : "", wide: true,
      body: h("div", { class: "stack" }, h("div", { class: "md-big" }, img),
        r.text ? h("div", {}, h("div", { class: "small muted" }, cfg.text.name), h("div", {}, r.text)) : null,
        h("div", { class: "row wrap", style: { gap: "6px" } }, Object.entries(r.facets).map(([f, v]) => h("span", { class: "chip" }, h("span", { class: "muted" }, f), String(v ?? "–"))),
          r.colors ? String(r.colors).split(/\s+/).filter(Boolean).map((c) => h("span", { class: "chip" }, h("span", { class: "md-dot", style: { background: (COLORS.find(([n]) => n === c) || [, "var(--muted)"])[1] } }), c)) : null),
        r.score != null ? h("div", { class: "small dim" }, "score ", h("b", {}, fmt.n(r.score, 4))) : null,
        h("div", { class: "small muted mono" }, "sha256 ", r.media?.sha256 || "–", h("br"), r.media?.uri || "")),
      foot: [h("button", { class: "btn primary", onclick: () => { d.close(); st.q = ""; queryImage = null; save(); drawTop(); search(r.key); } }, icon("sparkle"), "More like this"),
        h("button", { class: "btn", onclick: () => copy(r.key, "Copied the key") }, icon("copy"), "Copy key")],
    });
  }

  // ------------------------------------------------------------ the map
  async function drawMap() {
    if (!mapData || mapData.table !== st.table) {
      clear(body, h("div", { class: "rx-wait" }, h("span", { class: "spin" }), "Reading every row's vector and laying them out…"));
      try {
        const t = cfg.t, F = cfg.facets[0]?.name;
        const j = await api("/v1/sql", { body: { sql: `SELECT ${quoteName(t.short.key)}, ${quoteName(cfg.vec.name)}, ${quoteName(cfg.media.name)}${F ? ", " + quoteName(F) : ""} FROM ${quoteName(t.table)} LIMIT 2000` } });
        const vs = j.rows.map((r) => r[1]).filter((v) => Array.isArray(v));
        const pcs = principal(vs, 2);
        const mean = pcs.mean;
        const pts = j.rows.filter((r) => Array.isArray(r[1])).map((r) => ({ key: String(r[0]), xy: pcs.project(r[1]), media: parseMedia(r[2]), group: F ? String(r[3]) : "" }));
        mapData = { table: st.table, pts, pcs, mean, groups: byCount(pts.map((p) => p.group)), F };
      } catch (e) { clear(body, failure(e, "The vectors")); return; }
    }
    let qxy = null;
    if (res.q.params.v?.embed && typeof res.q.params.v.embed === "string") {
      try { const e = await api("/v1/embed", { body: { text: res.q.params.v.embed, table: cfg.t.table, field: cfg.vec.name } }); qxy = mapData.pcs.project(e.vector); } catch { /* no marker */ }
    } else if (res.like) qxy = mapData.pts.find((p) => p.key === res.like)?.xy;
    const W = 1000, H = 620, pad = 30;
    const xs = mapData.pts.map((p) => p.xy[0]), ys = mapData.pts.map((p) => p.xy[1]);
    const [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
    const X = (x) => pad + (x - x0) / ((x1 - x0) || 1) * (W - 2 * pad), Y = (y) => pad + (y - y0) / ((y1 - y0) || 1) * (H - 2 * pad);
    const rank = new Map(res.rows.map((r, i) => [r.key, i]));
    // the seven commonest groups get a colour each, the rest one grey: hues are never reused
    const color = (g) => { const i = mapData.groups.indexOf(g); return i >= 0 && i < 7 ? SERIES[i] : "var(--muted)"; };
    const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "md-map", role: "img", "aria-label": "every row's vector in two dimensions" });
    const ranked = mapData.pts.filter((p) => rank.has(p.key)).sort((a, b) => rank.get(b.key) - rank.get(a.key));
    for (const p of mapData.pts.filter((p) => !rank.has(p.key))) svgEl.append(dot(p, false));
    if (qxy) for (const p of ranked.slice(-10)) svgEl.append(s("line", { x1: X(qxy[0]), y1: Y(qxy[1]), x2: X(p.xy[0]), y2: Y(p.xy[1]), class: "md-ray" }));
    for (const p of ranked) svgEl.append(dot(p, true));
    if (qxy) svgEl.append(s("g", { class: "md-q", transform: `translate(${X(qxy[0])},${Y(qxy[1])})` }, s("circle", { r: 11 }), s("path", { d: "M-5 0H5M0 -5V5" })));
    function dot(p, hit) {
      const g = s("circle", { cx: X(p.xy[0]), cy: Y(p.xy[1]), r: hit ? 6.5 : 4, class: "md-pt" + (hit ? " hit" : ""), style: { fill: color(p.group) } });
      g.addEventListener("pointermove", (e) => {
        const im = h("img", { class: "md-tipimg", alt: "" });
        if (p.media?.sha256) fileUrl(p.media.sha256).then((u) => { if (u) im.src = u; });
        showTip(e.clientX, e.clientY, [h("div", { class: "t" }, p.key, rank.has(p.key) ? ` · #${rank.get(p.key) + 1}` : ""), im, mapData.F ? h("div", { class: "r" }, h("span", { class: "k box", style: { background: color(p.group) } }), h("span", { class: "n", style: { marginLeft: 0 } }, p.group)) : null]);
      });
      g.addEventListener("pointerleave", hideTip);
      g.addEventListener("click", () => { const r = res.rows.find((x) => x.key === p.key); if (r) open(r); else { st.q = ""; queryImage = null; drawTop(); search(p.key); } });
      return g;
    }
    clear(body, h("div", { class: "card md-mapcard" },
      h("div", { class: "hd" }, h("h3", {}, "The embedding map"), h("span", { class: "sub" }, `${fmt.plural(mapData.pts.length, "row")} by their vectors' two main directions (${fmt.pct(mapData.pcs.share, 0)} of the variance); the larger dots are the hits${qxy ? ", the cross the query" : ""}; click one for more like it`)),
      h("div", { class: "bd" }, mapData.F ? h("div", { class: "legend", style: { justifyContent: "center" } }, [...mapData.groups.slice(0, 7), ...(mapData.groups.length > 7 ? ["other"] : [])].map((g) => h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "md-dot", style: { background: color(g), border: 0 } }), g))) : null, svgEl)));
  }

  return () => { run++; };
}

/** Distinct values, the commonest first. */
function byCount(vs) {
  const n = new Map();
  for (const v of vs) n.set(v, (n.get(v) || 0) + 1);
  return [...n.keys()].sort((a, b) => n.get(b) - n.get(a) || String(a).localeCompare(String(b)));
}

/** The top principal directions of vectors, by power iteration with deflation: {project(v) → [x, y], share}. */
function principal(vs, k) {
  const n = vs.length, d = vs[0]?.length || 0;
  const mean = new Float64Array(d);
  for (const v of vs) for (let i = 0; i < d; i++) mean[i] += v[i] / n;
  const X = vs.map((v) => { const r = new Float64Array(d); for (let i = 0; i < d; i++) r[i] = v[i] - mean[i]; return r; });
  const total = X.reduce((a, r) => a + r.reduce((b, x) => b + x * x, 0), 0);
  const comps = [];
  let explained = 0;
  for (let c = 0; c < k; c++) {
    let w = new Float64Array(d).map(() => Math.random() - 0.5);
    for (let it = 0; it < 60; it++) {
      const nw = new Float64Array(d);
      for (const r of X) { let dot = 0; for (let i = 0; i < d; i++) dot += r[i] * w[i]; for (let i = 0; i < d; i++) nw[i] += dot * r[i]; }
      for (const p of comps) { let dot = 0; for (let i = 0; i < d; i++) dot += nw[i] * p[i]; for (let i = 0; i < d; i++) nw[i] -= dot * p[i]; }
      const norm = Math.hypot(...nw) || 1;
      w = nw.map((x) => x / norm);
    }
    let lam = 0;
    for (const r of X) { let dot = 0; for (let i = 0; i < d; i++) dot += r[i] * w[i]; lam += dot * dot; }
    explained += lam;
    comps.push(w);
  }
  return {
    share: total ? explained / total : 0,
    project: (v) => comps.map((w) => { let dot = 0; for (let i = 0; i < d; i++) dot += (v[i] - mean[i]) * w[i]; return dot; }),
  };
}
