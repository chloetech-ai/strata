// The feature store demo's page in ChloeDB's command center (#/app/features).
// The command center loads this module through chloedb-server, which
// forwards /v1/apps/features/... to the demo (featurestore.py), and runs
// render(el, ctx) with its own helpers (ctx.core, ctx.charts) and the
// demo's API (ctx.api). It imports nothing.
//
//   #/app/features            the pipeline, live: shoppers, features, models, serving
//   #/app/features/shop       the sample application: a shopper's ranking, explained; click to see features change
//   #/app/features/features   each feature's definition and spread; the tables, filtered
//   #/app/features/training   point-in-time training sets, models trained, compared, deployed
//   #/app/features/speed      serving and training-set timings, against Feast

export default { render };

const CSS = `
.fs-head { display: flex; gap: 10px 16px; align-items: center; flex-wrap: wrap; margin-bottom: 12px; }
.fs-head h2 { margin: 0; font-size: 18px; }
.fs-head .grow { flex: 1; }
.fs-kpis { display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 12px; margin-bottom: 14px; }
.fs-kpis .kpi { background: var(--surface); border: 1px solid var(--line); border-radius: var(--r); }
.fs-kpis .delta { max-width: none; }
.fs-pipe { width: 100%; min-width: 760px; height: auto; display: block; }
/* On a phone the diagram keeps a readable size and scrolls sideways. */
:has(> .fs-pipe) { overflow-x: auto; }
.fs-pipe .node rect { fill: var(--surface); stroke: var(--line-2); stroke-width: 1.2; }
.fs-pipe .node.k-stream rect { stroke: var(--s1); }
.fs-pipe .node.k-batch rect { stroke: var(--s4); }
.fs-pipe .node.k-train rect { stroke: var(--s3); }
.fs-pipe .node.k-serve rect { stroke: var(--s2); }
.fs-pipe .node .t { font-size: 13px; font-weight: 650; fill: var(--text); }
.fs-pipe .node .m { font-size: 11.5px; fill: var(--text-2); font-family: var(--mono); }
.fs-pipe .node .k { font-size: 10px; fill: var(--muted); text-transform: uppercase; letter-spacing: .05em; }
.fs-pipe .edge { fill: none; stroke: var(--axis); stroke-width: 1.6; }
.fs-pipe .flow { fill: none; stroke-width: 2.4; stroke-dasharray: 3 11; stroke-linecap: round; animation: fs-flow 1s linear infinite; }
.fs-pipe .flow.slow { animation-duration: 3.5s; }
.fs-pipe .flow.off { animation: none; opacity: .25; }
.fs-pipe .lbl { font-size: 10.5px; fill: var(--muted); }
@keyframes fs-flow { to { stroke-dashoffset: -14; } }
.fs-legend { display: flex; gap: 14px; flex-wrap: wrap; font-size: 12px; color: var(--text-2); margin-top: 6px; }
.fs-legend i { display: inline-block; width: 18px; height: 3px; border-radius: 2px; vertical-align: middle; margin-right: 6px; }
.fs-shop { display: grid; grid-template-columns: 290px minmax(0, 1fr); gap: 14px; align-items: start; }
.fs-who .big { font-size: 20px; font-weight: 650; }
.fs-who .truth { border: 1px dashed var(--line-2); border-radius: var(--r-sm); padding: 8px 10px; margin: 10px 0; font-size: 12.5px; }
.fs-who .truth b { color: var(--text); }
.fs-cats { display: grid; grid-template-columns: 70px minmax(0, 1fr) 54px; gap: 4px 8px; align-items: center; font-size: 12px; }
.fs-cats .bar { height: 8px; border-radius: 4px; background: var(--s1); }
.fs-cats .bg { height: 8px; border-radius: 4px; background: var(--surface-3); }
.fs-cats .like { font-weight: 650; color: var(--text); }
.fs-grid { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 12px; }
.fs-prod { border: 1px solid var(--line); border-radius: var(--r); background: var(--surface); padding: 10px 12px; display: flex; flex-direction: column; gap: 6px; position: relative; }
.fs-prod.liked { border-color: color-mix(in srgb, var(--s3) 60%, var(--line)); }
.fs-prod .rank { position: absolute; top: 8px; right: 10px; font-size: 11px; color: var(--muted); }
.fs-prod .ttl { font-weight: 650; padding-right: 18px; }
.fs-prod .meta { font-size: 12px; color: var(--text-2); display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
.fs-prod .sc { font-family: var(--mono); font-size: 12px; }
.fs-contrib { display: grid; grid-template-columns: 76px minmax(0, 1fr); gap: 2px 6px; align-items: center; font-size: 10.5px; color: var(--muted); }
.fs-contrib .axis { position: relative; height: 7px; background: var(--surface-2); border-radius: 3px; }
.fs-contrib .axis::after { content: ""; position: absolute; left: 50%; top: -1px; bottom: -1px; width: 1px; background: var(--axis); }
.fs-contrib .axis i { position: absolute; top: 0; bottom: 0; border-radius: 3px; }
.fs-wf { display: flex; height: 14px; border-radius: 4px; overflow: hidden; background: var(--surface-3); margin: 6px 0 4px; }
.fs-wf > i { display: block; height: 100%; }
.fs-note { border-left: 3px solid var(--s3); background: var(--surface-2); padding: 8px 12px; border-radius: 0 var(--r-sm) var(--r-sm) 0; font-size: 13px; margin-bottom: 12px; }
.fs-note.warn { border-left-color: var(--s2); }
.fs-note code { font-family: var(--mono); font-size: 12px; }
.fs-feat { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 12px; }
.fs-fc { border: 1px solid var(--line); border-radius: var(--r); background: var(--surface); padding: 10px 12px; }
.fs-fc .nm { font-family: var(--mono); font-weight: 650; }
.fs-fc .f { font-family: var(--mono); font-size: 11.5px; color: var(--text-2); margin: 4px 0 6px; overflow-wrap: anywhere; }
.fs-fc .what { font-size: 12px; color: var(--muted); min-height: 30px; }
.fs-hist { width: 100%; height: 54px; display: block; margin-top: 6px; }
.fs-hist rect { fill: var(--s1); }
.fs-fc.k-batch .fs-hist rect { fill: var(--s4); }
.fs-kind { font-size: 10.5px; text-transform: uppercase; letter-spacing: .05em; padding: 1px 7px; border-radius: 999px; font-weight: 650; }
.fs-kind.streaming { background: color-mix(in srgb, var(--s1) 16%, transparent); color: var(--s1); }
.fs-kind.batch { background: color-mix(in srgb, var(--s4) 18%, transparent); color: var(--s4); }
.fs-browse { display: grid; grid-template-columns: 180px minmax(0, 1fr) 200px auto; gap: 8px; margin-bottom: 10px; }
.fs-browse .input, .fs-browse .select { font-family: var(--mono); font-size: 12.5px; }
.fs-tl { width: 100%; height: 260px; display: block; }
.fs-tl .grid { stroke: var(--grid); }
.fs-tl .ax { font-size: 10.5px; fill: var(--muted); }
.fs-tl .step { fill: none; stroke: var(--s1); stroke-width: 2; }
.fs-tl .latest { stroke: var(--s2); stroke-width: 1.6; stroke-dasharray: 5 4; }
.fs-tl .pick { stroke: var(--s3); stroke-width: 1; opacity: .55; }
.fs-tl .imp { fill: var(--surface); stroke: var(--text-2); stroke-width: 1.2; }
.fs-tl .imp.c { fill: var(--s3); stroke: var(--s3); }
.fs-tl .snap { fill: var(--s1); }
.fs-sql2 { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }
.fs-sql2 pre.code { font-size: 11.5px; white-space: pre-wrap; margin: 6px 0 10px; max-height: 260px; overflow: auto; }
.fs-auc { display: grid; grid-template-columns: 120px minmax(0, 1fr) 52px; gap: 6px 10px; align-items: center; font-size: 12.5px; }
.fs-auc .b { height: 12px; border-radius: 3px; }
.fs-auc .bg { background: var(--surface-3); border-radius: 3px; position: relative; height: 12px; }
.fs-auc .bg > i { position: absolute; left: 0; top: 0; bottom: 0; border-radius: 3px; }
.fs-vs { width: 100%; border-collapse: collapse; font-size: 13px; }
.fs-vs td, .fs-vs th { padding: 6px 8px; border-top: 1px solid var(--line); text-align: left; }
.fs-vs th { font-weight: 600; color: var(--text-2); font-size: 12px; }
.fs-vs td.num, .fs-vs th.num { text-align: right; font-variant-numeric: tabular-nums; }
.fs-vs .barcell { width: 40%; }
.fs-vs .barcell i { display: block; height: 10px; border-radius: 3px; }
.fs-flash { animation: fs-flash 1.6s ease-out; }
@keyframes fs-flash { from { background: color-mix(in srgb, var(--s3) 30%, transparent); } to { background: transparent; } }
@media (max-width: 1100px) { .fs-kpis { grid-template-columns: repeat(3, minmax(0, 1fr)); } .fs-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
@media (max-width: 900px) {
  .fs-kpis { grid-template-columns: repeat(2, minmax(0, 1fr)); }
  .fs-shop, .fs-sql2, .fs-browse { grid-template-columns: minmax(0, 1fr); }
}
@media (max-width: 560px) { .fs-grid { grid-template-columns: minmax(0, 1fr); } }
`;

const TABS = [["", "Pipeline"], ["builder", "Builder"], ["shop", "Shop"], ["features", "Features"], ["training", "Training"], ["speed", "Speed"]];
const FEAT_COLORS = { u_ctr: "var(--s1)", i_ctr: "var(--s2)", i_trend: "var(--s4)", uc_ctr: "var(--s3)", uc_share: "var(--s5)", log_views: "var(--s6)", price_ratio: "var(--s7)" };

function render(el, ctx) {
  const { h } = ctx.core;
  el.append(h("style", {}, CSS));
  const head = h("div", { class: "fs-head" });
  const tabs = h("div", { class: "tabs", role: "tablist" });
  const body = h("div");
  el.append(head, tabs, body);
  let state = null;
  let stopPage = null;
  let listeners = [];
  const onState = (f) => { listeners.push(f); if (state) f(state); };

  // The header: what runs, and its controls.
  const pill = h("span", { class: "pill" });
  const modelChip = h("span", { class: "badge" });
  const pause = h("button", { class: "btn sm" });
  const rate = h("div", { class: "seg", title: "shopper sessions a second" });
  for (const r of [4, 12, 30]) {
    const b = h("button", { type: "button", "data-r": r, onclick: () => ctx.api("/api/sim", { method: "POST", body: { rate: r } }).then(poll) }, `${r}/s`);
    rate.append(b);
  }
  pause.onclick = () => ctx.api("/api/sim", { method: "POST", body: { running: !(state && state.running) } }).then(poll);
  head.append(h("h2", {}, "Feature store"), h("span", { class: "muted small" }, "a shop ranked by a model, on features ChloeDB keeps"), h("span", { class: "grow" }), pill, modelChip, rate, pause);
  const drawHead = (s) => {
    pill.className = "pill " + (s.error ? "warn" : s.running ? "ok" : "");
    pill.textContent = s.phase === "backfill" ? "making history…" : s.running ? `${ctx.core.fmt.si(s.rates.requests)} shoppers/s` : "paused";
    if (s.error) pill.title = s.error;
    modelChip.textContent = s.model ? `serving ${s.model.name}` : "serving random";
    pause.textContent = s.running ? "Pause shoppers" : "Resume shoppers";
    for (const b of rate.children) b.setAttribute("aria-pressed", String(Number(b.dataset.r) === s.rate));
  };
  listeners.push(drawHead);

  const show = (path) => {
    const tab = path[0] || "";
    for (const b of tabs.children) b.setAttribute("aria-selected", String(b.dataset.tab === tab));
    try { stopPage?.(); } catch (e) { console.warn(e); }
    listeners = [drawHead];
    body.replaceChildren();
    const page = { "": pipelinePage, builder: builderPage, shop: shopPage, features: featuresPage, training: trainingPage, speed: speedPage }[tab] || pipelinePage;
    stopPage = page(body, ctx, onState, poll);
  };
  for (const [id, label] of TABS) {
    tabs.append(h("button", { type: "button", role: "tab", "data-tab": id, onclick: () => { location.hash = id ? ctx.href(id) : ctx.href(); } }, label));
  }
  let timer = null, gone = false;
  async function poll() {
    try {
      state = await ctx.api("/api/state", { timeout: 8000 });
      for (const f of listeners) { try { f(state); } catch (e) { console.warn(e); } }
    } catch (e) {
      pill.className = "pill bad"; pill.textContent = "the demo does not answer"; pill.title = String(e.message || e);
    }
  }
  const loop = async () => { if (gone) return; await poll(); timer = setTimeout(loop, 1500); };
  ctx.onPath(show);
  show(ctx.path());
  loop();
  return () => { gone = true; clearTimeout(timer); try { stopPage?.(); } catch (e) { console.warn(e); } };
}

// ------------------------------------------------------------- helpers

function kpi(h, label, sub) {
  const val = h("div", { class: "val" }, "–"), delta = h("div", { class: "delta" }, sub || "");
  const e = h("div", { class: "kpi" }, h("div", { class: "lbl" }, label), val, delta);
  e.set = (v, d) => { val.replaceChildren(...(Array.isArray(v) ? v : [v])); if (d != null) delta.textContent = d; };
  return e;
}

function every(ms, f) {
  let t = null, gone = false;
  const go = async () => { if (gone) return; try { await f(); } catch (e) { console.warn(e); } if (!gone) t = setTimeout(go, ms); };
  go();
  return () => { gone = true; clearTimeout(t); };
}

/** Per model, the click rate over a sliding 40 s window, every 10 s. */
function ctrPoints(rows) {
  const byT = new Map(), names = new Map();
  for (const r of rows) {
    if (!byT.has(r.t)) byT.set(r.t, new Map());
    byT.get(r.t).set(r.name, [r.shown, r.clicked]);
    names.set(r.name, Math.max(names.get(r.name) || 0, r.t));
  }
  const ts = [...byT.keys()].sort((a, b) => a - b);
  const pts = ts.map((t) => {
    const p = { t };
    for (const n of names.keys()) {
      let s = 0, c = 0;
      for (const u of ts) if (u <= t && u > t - 40000) { const v = byT.get(u).get(n); if (v) { s += v[0]; c += v[1]; } }
      p[n] = s >= 40 ? c / s : null;
    }
    return p;
  });
  // The models seen most recently first; random always.
  const order = [...names.entries()].sort((a, b) => b[1] - a[1]).map((x) => x[0]);
  const keep = order.filter((n) => n !== "random").slice(0, 4);
  if (names.has("random")) keep.push("random");
  return { pts: pts.slice(0, -1), names: keep };
}

function ctrChart(ctx, host) {
  const { h } = ctx.core;
  let chart = null, key = "";
  const wrap = h("div");
  host.append(wrap);
  const draw = async () => {
    const r = await ctx.api("/api/ctr?minutes=15");
    const { pts, names } = ctrPoints(r.rows);
    const k = names.join(",");
    if (!chart || k !== key) {
      key = k;
      chart = ctx.charts.timeChart({ height: 210, fmt: (v) => ctx.core.fmt.pct(v, 1), min: 0, legend: true,
        threshold: r.random_ctr != null ? { value: r.random_ctr, label: "random ranking, all history:" } : null,
        series: names.map((n, i) => ({ name: n === "random" ? "random ranking" : n, color: n === "random" ? "var(--axis)" : ["var(--s3)", "var(--s1)", "var(--s5)", "var(--s4)"][i % 4], get: (p) => p[n], dash: n === "random" })) });
      wrap.replaceChildren(chart);
    }
    chart.update(pts);
  };
  return every(5000, draw);
}

function latencyChart(ctx, host, onState) {
  const chart = ctx.charts.timeChart({ height: 210, fmt: (v) => ctx.core.fmt.ms(v), min: 0,
    series: [
      { name: "serving p50 (4 reads by key, 489 rows, transforms, model)", color: "var(--s2)", get: (p) => p.serve_p50 },
      { name: "serving p99", color: "var(--s5)", get: (p) => p.serve_p99, dash: true },
      { name: "write p50 (impressions + streaming features, one transaction)", color: "var(--s1)", get: (p) => p.write_p50 },
      { name: "click to feature p50", color: "var(--s3)", get: (p) => p.fresh_p50 },
    ] });
  host.append(chart);
  onState((s) => chart.update(s.series));
}

// ------------------------------------------------------------- pipeline

const W = 170, HH = 66;
const NODES = {
  shoppers: { x: 20, y: 150, t: "Shoppers", k: "simulated", c: "" },
  imps: { x: 230, y: 44, t: "Impressions", k: "events + labels", c: "" },
  stream: { x: 440, y: 44, t: "Streaming transforms", k: "on write", c: "k-stream" },
  batch: { x: 440, y: 140, t: "Batch transform", k: "SQL every 10 s", c: "k-batch" },
  online: { x: 650, y: 44, t: "Online features", k: "4 tables, by key", c: "k-stream" },
  snaps: { x: 860, y: 44, t: "Snapshots", k: "every 2 s", c: "" },
  asof: { x: 860, y: 262, t: "Training set", k: "ASOF JOIN", c: "k-train" },
  train: { x: 650, y: 262, t: "Model training", k: "logistic regression", c: "k-train" },
  models: { x: 440, y: 262, t: "Models", k: "rows of fs_models", c: "k-train" },
  serve: { x: 230, y: 262, t: "Serving", k: "reads by key + model", c: "k-serve" },
};
// [from, to, colour, path, label, label x, label y]
const A = 77, C = 295; // the middles of the two rows
const EDGES = [
  ["shoppers", "imps", "s1", `M105,150 C105,${A} 160,${A} 230,${A}`],
  ["imps", "stream", "s1", `M400,${A} L440,${A}`],
  ["imps", "batch", "s4", `M400,${A} C422,${A} 418,173 440,173`],
  ["stream", "online", "s1", `M610,${A} L650,${A}`],
  ["batch", "online", "s4", `M610,173 C632,173 628,${A} 650,${A}`],
  ["online", "snaps", "s1", `M820,${A} L860,${A}`],
  ["snaps", "asof", "s3", `M945,110 L945,262`],
  ["imps", "asof", "s3", `M315,44 V22 H1052 V${C} H1030`, "labels: each impression's click, joined as of its time", 640, 16],
  ["asof", "train", "s3", `M860,${C} L820,${C}`],
  ["train", "models", "s3", `M650,${C} L610,${C}`],
  ["models", "serve", "s2", `M440,${C} L400,${C}`],
  ["online", "serve", "s2", `M735,110 V236 H315 V262`, "features by key", 520, 230],
  ["serve", "shoppers", "s2", `M230,${C} C105,${C} 105,${C - 20} 105,216`, "top 8", 150, 322],
];

function pipelineDiagram(ctx) {
  const { s } = ctx.core;
  const svg = s("svg", { class: "fs-pipe", viewBox: "0 0 1070 340", role: "img", "aria-label": "the feature pipeline" });
  const flows = {};
  for (const [a, b, c, d, lbl, lx, ly] of EDGES) {
    svg.append(s("path", { class: "edge", d }));
    const f = s("path", { class: "flow", d, style: `stroke: var(--${c})` });
    flows[a + ">" + b] = f;
    svg.append(f);
    if (lbl) svg.append(s("text", { class: "lbl", x: lx, y: ly, "text-anchor": "middle" }, lbl));
  }
  const metric = {};
  for (const [id, n] of Object.entries(NODES)) {
    const m = s("text", { class: "m", x: n.x + 12, y: n.y + 55 }, "–");
    metric[id] = m;
    svg.append(s("g", { class: "node " + n.c },
      s("rect", { x: n.x, y: n.y, width: W, height: HH, rx: 9 }),
      s("text", { class: "k", x: n.x + 12, y: n.y + 17 }, n.k),
      s("text", { class: "t", x: n.x + 12, y: n.y + 35 }, n.t), m));
  }
  svg.update = (st, extra) => {
    const f = ctx.core.fmt;
    const j = st.jobs || {};
    const on = st.running && st.phase === "live";
    metric.shoppers.textContent = `${f.si(st.rates.requests)} sessions/s`;
    metric.imps.textContent = `${f.si(st.counts.impressions)} · ${f.si(st.rates.impressions)}/s`;
    metric.stream.textContent = `write p50 ${f.ms(st.write.p50)}`;
    metric.batch.textContent = j.trend ? `${f.ms(j.trend.ms)} · ${f.ago(j.trend.t)}` : "–";
    metric.online.textContent = `fresh in ${f.ms(st.fresh.p50)}`;
    metric.snaps.textContent = `${f.si(st.counts.snap_rows)} rows · ${j.snapshot ? f.ms(j.snapshot.ms) : "–"}`;
    metric.asof.textContent = st.model ? `${f.si(st.model.train_rows)} rows · ${f.ms(st.model.build_ms)}` : "not built yet";
    metric.train.textContent = st.model ? `AUC ${st.model.auc_true?.toFixed(3)} · ${f.ms(st.model.train_ms)}` : "no model yet";
    metric.models.textContent = extra?.models != null ? `${extra.models} · ${st.model ? st.model.name : "random"}` : (st.model ? st.model.name : "random");
    metric.serve.textContent = `p50 ${f.ms(st.serve.p50)}`;
    // Training runs when asked: its edges move for 20 s after a model is made.
    const training = st.model && Date.now() - st.model.created < 20000;
    const trainEdges = new Set(["snaps>asof", "imps>asof", "asof>train", "train>models"]);
    for (const [k, p] of Object.entries(flows)) {
      const off = !on || (trainEdges.has(k) && !training) || ((k === "online>serve" || k === "models>serve") && !st.model);
      p.classList.toggle("off", off);
      p.classList.toggle("slow", k === "imps>batch" || k === "batch>online" || k === "online>snaps");
    }
  };
  return svg;
}

function pipelinePage(root, ctx, onState) {
  const { h, card, fmt } = ctx.core;
  const kRate = kpi(h, "Impressions written", "a row per product shown");
  const kServe = kpi(h, "Serving p50", "the shopper, 240 products, their categories");
  const kFresh = kpi(h, "Click to feature", "a click in the counters, p50");
  const kAuc = kpi(h, "Deployed model", "AUC on the last minute, as served");
  const kCtr = kpi(h, "Click rate, last 2 min", "");
  root.append(h("div", { class: "fs-kpis" }, kRate, kServe, kFresh, kAuc, kCtr));
  const note = h("div", { class: "fs-note" });
  const diag = pipelineDiagram(ctx);
  root.append(card({ title: "The pipeline, live", sub: "one ChloeDB server holds every box", body: [note, diag, h("div", { class: "fs-legend" },
    h("span", {}, h("i", { style: { background: "var(--s1)" } }), "streaming: on write"),
    h("span", {}, h("i", { style: { background: "var(--s4)" } }), "batch: SQL on a schedule"),
    h("span", {}, h("i", { style: { background: "var(--s3)" } }), "training: point in time"),
    h("span", {}, h("i", { style: { background: "var(--s2)" } }), "serving: reads by key"))] }));
  const charts = h("div", { class: "grid g2", style: { marginTop: "14px" } });
  const ctrHost = h("div"), latHost = h("div");
  charts.append(card({ title: "Click rate by model", sub: "from fs_impressions, a 40 s window", body: ctrHost }), card({ title: "Latency", sub: "every 5 s", body: latHost }));
  root.append(charts);
  const stopCtr = ctrChart(ctx, ctrHost);
  latencyChart(ctx, latHost, onState);
  let models = null, ctr2 = null;
  const stopModels = every(6000, async () => {
    const m = await ctx.api("/api/models");
    models = m.models.length;
    const c = await ctx.api("/api/ctr?minutes=2");
    const by = {};
    for (const r of c.rows) { const x = by[r.name] || (by[r.name] = [0, 0]); x[0] += r.shown; x[1] += r.clicked; }
    ctr2 = by; ctr2.base = c.random_ctr;
  });
  onState((st) => {
    kRate.set([fmt.si(st.counts.impressions), h("small", {}, `${fmt.si(st.rates.impressions)}/s`)], `${fmt.si(st.counts.clicks)} clicks, the labels`);
    kServe.set(fmt.ms(st.serve.p50), `p99 ${fmt.ms(st.serve.p99)}; 489 rows by key a request`);
    kFresh.set(fmt.ms(st.fresh.p50), `p99 ${fmt.ms(st.fresh.p99)}; the next ranking sees it`);
    kAuc.set(st.model ? st.model.auc_true.toFixed(3) : "random", st.model ? `${st.model.name}, ${st.model.join_kind}` : "train one on the Training tab");
    if (ctr2) {
      const cur = st.model ? st.model.name : "random";
      const a = ctr2[cur], b = ctr2.random;
      const r = a && a[0] ? a[1] / a[0] : null, rr = b && b[0] ? b[1] / b[0] : null;
      const base = rr != null ? rr : ctr2.base;
      kCtr.set(fmt.pct(r), cur === "random" ? "random ranking" : base != null ? `${cur}; random ranking ${fmt.pct(base)}${r && base ? `, ×${(r / base).toFixed(1)}` : ""}` : cur);
    }
    diag.update(st, { models });
    note.replaceChildren(st.model
      ? h("span", {}, "Shoppers see products ranked by ", h("b", {}, st.model.name), ` (${st.model.join_kind} training set). Every impression writes its streaming features in its own transaction; the batch job and the snapshots run beside it; the Training tab builds the next model from the same tables.`)
      : h("span", {}, "Shoppers see products in a random order, which is how the history was made. Train a model on the ", h("a", { href: ctx.href("training") }, "Training"), " tab and deploy it: the click rate below moves when it serves."));
  });
  return () => { stopCtr(); stopModels(); };
}

// ------------------------------------------------------------- shop

function contribBars(h, contrib) {
  const max = 1.2;
  const rows = [];
  for (const [k, v] of Object.entries(contrib)) {
    const w = Math.min(50, Math.abs(v) / max * 50);
    rows.push(h("span", {}, k), h("div", { class: "axis", title: `${k}: ${v >= 0 ? "+" : ""}${v.toFixed(3)} to the log-odds` },
      h("i", { style: { left: v >= 0 ? "50%" : `${50 - w}%`, width: `${w}%`, background: v >= 0 ? "var(--s3)" : "var(--s2)" } })));
  }
  return h("div", { class: "fs-contrib" }, rows);
}

function shopPage(root, ctx) {
  const { h, card, fmt } = ctx.core;
  let users = [], cats = [], uid = null, last = null;
  const pick = h("select", { class: "select", style: { width: "100%" } });
  const again = h("button", { class: "btn sm", onclick: () => { uid = users[Math.floor(Math.random() * users.length)].id; pick.value = uid; load(); } }, "Another shopper");
  const who = h("div", { class: "fs-who" });
  const grid = h("div", { class: "fs-grid" });
  const timing = h("div");
  const result = h("div"), failed = h("div");
  root.append(h("div", { class: "fs-note" }, "The sample application: the shop's ranking for one shopper, as the shoppers get it. Each card shows how much each feature pushed the product up or down. Click a product as this shopper: its impressions and the click are written in one transaction, and the shopper's features are read back at once."));
  root.append(h("div", { class: "fs-shop" },
    card({ title: "Shopper", tools: [again], body: [pick, who] }),
    h("div", {}, card({ title: "Ranked for them", sub: "top 8 of 240", body: [timing, failed, result, grid] }))));
  pick.onchange = () => { uid = pick.value; load(); };

  async function load(flash) {
    let r;
    try { r = await ctx.api(`/api/shop?user=${encodeURIComponent(uid)}`); } catch (e) {
      // The shop could not rank (a table the model reads is gone): say so
      // in place of the products, not a blank page.
      failed.replaceChildren(h("div", { class: "error" }, `The shop could not rank: ${e.message}`));
      grid.replaceChildren();
      return;
    }
    failed.replaceChildren();
    last = r;
    drawWho(r, flash);
    const ms = r.ms;
    const segs = [["fetch", "var(--s1)", "4 reads by key"], ["transform", "var(--s4)", "transforms"], ["score", "var(--s3)", "model"]];
    timing.replaceChildren(
      h("div", { class: "row between small" }, h("span", {}, `${r.model === "random" ? "random ranking (no model deployed)" : "model " + (r.model_name || r.model)} · ${fmt.ms(ms.total)} in all`),
        h("span", { class: "muted" }, segs.map(([k, , l]) => `${l} ${fmt.ms(ms[k])}`).join(" · "))),
      h("div", { class: "fs-wf" }, segs.map(([k, c, l]) => h("i", { style: { width: `${ms.total ? ms[k] / ms.total * 100 : 0}%`, background: c }, title: `${l}: ${fmt.ms(ms[k])}` }))));
    grid.replaceChildren(...r.items.map((e, i) => {
      const liked = r.user.likes.includes(e.category);
      const b = h("button", { class: "btn sm primary", onclick: () => click(e.id, b) }, "Click");
      return h("div", { class: "fs-prod" + (liked ? " liked" : "") },
        h("span", { class: "rank" }, `#${i + 1}`),
        h("div", { class: "ttl" }, e.title),
        h("div", { class: "meta" }, h("span", { class: "badge" }, e.category), `$${e.price.toFixed(2)}`, liked ? h("span", { class: "pill good" }, "a liked category") : null),
        e.score != null ? h("div", { class: "sc" }, `p(click) ${fmt.pct(e.score, 1)}`) : null,
        e.contrib ? contribBars(h, e.contrib) : null,
        h("div", { class: "row" }, b));
    }));
  }

  function drawWho(r, flash) {
    const u = r.user, on = r.online || {};
    const max = Math.max(1, ...cats.map((c) => (r.usercat[c] || {}).clicks || 0));
    who.replaceChildren(
      h("div", { class: "big", style: { marginTop: "10px" } }, u.name, h("span", { class: "muted small" }, "  " + u.id)),
      h("div", { class: "truth" }, h("div", { class: "muted" }, "The simulator's truth, hidden from the model:"), "likes ", h("b", {}, u.likes.join(" and ")), `, minds prices ${u.price_sens > 1 ? "a lot" : u.price_sens > 0.6 ? "some" : "little"}`),
      h("div", { class: "small muted", style: { marginBottom: "6px" } }, "What the features know (fs_user_online, fs_usercat_online):"),
      h("div", { class: "row small" + (flash ? " fs-flash" : ""), style: { marginBottom: "8px" } }, h("span", { class: "badge" }, `${fmt.n(on.views)} views`), h("span", { class: "badge" }, `${fmt.n(on.clicks)} clicks`),
        h("span", { class: "badge" }, on.clicks ? `clicked $${(on.click_price / on.clicks).toFixed(0)} on average` : "no clicks yet")),
      h("div", { class: "fs-cats" }, cats.flatMap((c) => {
        const x = r.usercat[c] || {};
        const cl = x.clicks || 0;
        return [h("span", { class: u.likes.includes(c) ? "like" : "" }, c), h("div", { class: "bg" }, h("div", { class: "bar", style: { width: `${cl / max * 100}%`, background: u.likes.includes(c) ? "var(--s3)" : "var(--s1)" } })),
          h("span", { class: "muted num" }, `${cl}/${x.views || 0}`)];
      })));
  }

  async function click(item, btn) {
    btn.disabled = true;
    const r = await ctx.api("/api/click", { method: "POST", body: { user: uid, shown: last.items.map((e) => e.id), item, model: last.model } });
    const b = r.before || {}, a = r.after || {}, bu = r.before_uc || {}, au = r.after_uc || {};
    result.replaceChildren(h("div", { class: "fs-note fs-flash" },
      `Wrote ${r.ops} operations (8 impressions, the click, the streaming features) in one transaction: ${fmt.ms(r.write_ms)}. Read back by key in ${fmt.ms(r.read_ms)}: `,
      h("code", {}, `views ${b.views ?? 0} → ${a.views}, clicks ${b.clicks ?? 0} → ${a.clicks}, ${r.category} clicks ${bu.clicks ?? 0} → ${au.clicks}`),
      ". The ranking below is the next one, with them."));
    await load(true);
  }

  ctx.api("/api/users").then((r) => {
    users = r.users; cats = r.categories;
    pick.replaceChildren(...users.map((u) => h("option", { value: u.id }, `${u.name} (${u.id}) · likes ${u.likes.join(", ")}`)));
    uid = users[5].id; pick.value = uid;
    load();
  });
  return () => {};
}

// ------------------------------------------------------------- features

function hist(ctx, counts) {
  const { s } = ctx.core;
  const max = Math.max(1, ...counts);
  const w = 100 / counts.length;
  return s("svg", { class: "fs-hist", viewBox: "0 0 100 40", preserveAspectRatio: "none" },
    counts.map((c, i) => s("rect", { x: i * w + 0.3, width: w - 0.6, y: 40 - c / max * 38, height: c / max * 38 })));
}

function featuresPage(root, ctx) {
  const { h, card, fmt, table } = ctx.core;
  const srcHost = h("div"), featHost = h("div", { class: "fs-feat" }), browseHost = h("div");
  root.append(card({ title: "Feature tables", sub: "what the transforms read; each a ChloeDB table, read by key when serving", body: srcHost, flush: true }));
  root.append(h("div", { style: { height: "14px" } }));
  root.append(card({ title: "Features the model reads", sub: "one definition each, for training and serving; spread over 3,000 (shopper, product) pairs as serving sees them now", body: featHost }));
  root.append(h("div", { style: { height: "14px" } }));
  root.append(card({ title: "View and filter", sub: "any feature table, snapshot or the impressions, with a WHERE", body: browseHost }));

  const draw = async () => {
    const r = await ctx.api("/api/features");
    srcHost.replaceChildren(table([
      { key: "table", label: "table", render: (x) => h("code", {}, x.table) },
      { key: "kind", label: "kind", render: (x) => h("span", { class: "fs-kind " + x.kind }, x.kind) },
      { key: "key", label: "key" },
      { key: "rows", label: "rows", num: true, render: (x) => fmt.n(x.rows) },
      { key: "columns", label: "columns", render: (x) => x.columns.join(", ") },
      { key: "how", label: "how it is kept", render: (x) => h("span", { class: "small" }, x.how, x.last ? h("span", { class: "muted" }, ` (last: ${fmt.ms(x.last.ms)}, ${fmt.ago(x.last.t)})`) : null) },
    ], { rows: r.sources, compact: true }));
    featHost.replaceChildren(...r.features.map((f) => h("div", { class: "fs-fc k-" + f.kind },
      h("div", { class: "row between" }, h("span", { class: "nm" }, f.name), h("span", { class: "fs-kind " + f.kind }, f.kind)),
      h("div", { class: "f" }, f.formula),
      h("div", { class: "what" }, f.what),
      hist(ctx, f.hist),
      h("div", { class: "row between small muted" }, h("span", {}, fmt.si(f.lo, 2)), h("span", {}, `mean ${fmt.si(f.mean, 3)}`), h("span", {}, fmt.si(f.hi, 2))),
      f.weight != null ? h("div", { class: "small", style: { marginTop: "4px" } }, `weight in the deployed model: `, h("b", { style: { color: f.weight >= 0 ? "var(--s3)" : "var(--s2)" } }, (f.weight >= 0 ? "+" : "") + f.weight.toFixed(3))) : null,
      f.cleaning ? h("div", { class: "small", style: { marginTop: "4px" } }, `cleaned: ${fmt.n(f.cleaning.missing)} of ${fmt.n(f.cleaning.n)} missing (no clicks yet) set to 0, ${fmt.n(f.cleaning.clipped)} clipped to ±2`) : null)));
  };
  draw();

  // Browse.
  const T = ["fs_user_online", "fs_item_online", "fs_usercat_online", "fs_item_trend", "fs_user_snap", "fs_item_snap", "fs_usercat_snap", "fs_impressions", "fs_items", "fs_models"];
  const EX = { fs_user_online: ["clicks >= 20", "clicks DESC"], fs_item_online: ["views > 0", "clicks DESC"], fs_usercat_online: ["id LIKE 'u005|%'", "clicks DESC"], fs_item_trend: ["clicks_2m > 3", "clicks_2m DESC"],
    fs_user_snap: ["usr = 'u005'", "ts DESC"], fs_item_snap: ["item = 'p010'", "ts DESC"], fs_usercat_snap: ["uc = 'u005|games'", "ts DESC"], fs_impressions: ["clicked = 1 AND price > 100", "ts DESC"], fs_items: ["category = 'audio'", "price"], fs_models: ["", "created DESC"] };
  const sel = h("select", { class: "select" }, T.map((t) => h("option", { value: t }, t)));
  const where = h("input", { class: "input", placeholder: "WHERE …" });
  const order = h("input", { class: "input", placeholder: "ORDER BY …" });
  const run = h("button", { class: "btn primary" }, "Run");
  const out = h("div");
  const setEx = () => { const e = EX[sel.value] || ["", ""]; where.value = e[0]; order.value = e[1]; };
  sel.onchange = () => { setEx(); go(); };
  setEx();
  const go = async () => {
    try {
      const r = await ctx.api("/api/browse", { method: "POST", body: { table: sel.value, where: where.value, order: order.value, limit: 30 } });
      const cols = r.columns.filter((c) => !["weights", "sql", "features"].includes(c));
      out.replaceChildren(h("div", { class: "row between small", style: { marginBottom: "6px" } }, h("code", {}, r.sql), h("span", { class: "muted" }, `${fmt.n(r.count)} rows match · ${fmt.ms(r.ms)}`)),
        table(cols.map((c) => ({ key: c, label: c, num: typeof r.rows[0]?.[c] === "number", render: (x) => x[c] == null ? "–" : typeof x[c] === "number" && !Number.isInteger(x[c]) ? fmt.si(x[c], 3) : c === "ts" || c === "created" ? fmt.time(x[c]) : String(x[c]) })), { rows: r.rows, compact: true, maxHeight: "360px" }));
    } catch (e) { out.replaceChildren(h("div", { class: "fs-note warn" }, String(e.message || e))); }
  };
  run.onclick = go;
  where.onkeydown = order.onkeydown = (e) => { if (e.key === "Enter") go(); };
  browseHost.append(h("div", { class: "fs-browse" }, sel, where, order, run), out);
  go();
  return () => {};
}

// ------------------------------------------------------------- training

function timelineSvg(ctx, r) {
  const { s, fmt } = ctx.core;
  const Wd = 1000, Ht = 260, M = { l: 44, r: 120, t: 14, b: 26 };
  const svg = s("svg", { class: "fs-tl", viewBox: `0 0 ${Wd} ${Ht}`, preserveAspectRatio: "none" });
  const imps = r.impressions, snaps = r.snaps;
  if (!imps.length) { svg.append(s("text", { x: Wd / 2, y: Ht / 2, "text-anchor": "middle", class: "ax" }, "no impressions for this shopper in the last minutes")); return svg; }
  const t0 = r.since, t1 = r.now;
  const latest = imps[0].latest_clicks ?? 0;
  const ymax = Math.max(1, latest, ...snaps.map((x) => x.clicks)) * 1.08;
  const ymin = Math.min(...snaps.map((x) => x.clicks), ...imps.map((x) => x.u_clicks ?? 0)) * 0.95;
  const X = (t) => M.l + (t - t0) / (t1 - t0) * (Wd - M.l - M.r), Y = (v) => Ht - M.b - (v - ymin) / Math.max(1, ymax - ymin) * (Ht - M.t - M.b);
  for (let i = 0; i <= 4; i++) {
    const v = ymin + (ymax - ymin) * i / 4;
    svg.append(s("line", { class: "grid", x1: M.l, x2: Wd - M.r, y1: Y(v), y2: Y(v) }), s("text", { class: "ax", x: M.l - 6, y: Y(v) + 3, "text-anchor": "end" }, fmt.si(v)));
  }
  for (let i = 0; i <= 6; i++) {
    const t = t0 + (t1 - t0) * i / 6;
    svg.append(s("text", { class: "ax", x: X(t), y: Ht - 8, "text-anchor": "middle" }, fmt.time(t)));
  }
  // The shopper's clicks as the snapshots saw them: a step line.
  let d = "", prev = null;
  for (const x of snaps) {
    if (x.ts < t0) { prev = x; continue; }
    if (!d) d = `M${X(t0)},${Y((prev || x).clicks)}`;
    d += ` L${X(x.ts)},${Y(prev ? prev.clicks : x.clicks)} L${X(x.ts)},${Y(x.clicks)}`;
    prev = x;
  }
  if (prev) d += ` L${X(t1)},${Y(prev.clicks)}`;
  svg.append(s("path", { class: "step", d }));
  svg.append(s("line", { class: "latest", x1: M.l, x2: Wd - M.r, y1: Y(latest), y2: Y(latest) }), s("text", { class: "ax", x: Wd - M.r + 6, y: Y(latest) + 3, style: "fill: var(--s2)" }, `latest: ${latest} clicks`));
  svg.append(s("text", { class: "ax", x: Wd - M.r + 6, y: Y(latest) + 16, style: "fill: var(--s2)" }, "what a plain join gives"), s("text", { class: "ax", x: Wd - M.r + 6, y: Y(latest) + 28, style: "fill: var(--s2)" }, "every row: the future"));
  for (const x of imps) {
    const c = s("circle", { class: "imp" + (x.label ? " c" : ""), cx: X(x.ts), cy: Y(x.u_clicks ?? 0), r: x.label ? 4.5 : 3 });
    c.append(s("title", {}, `${fmt.time(x.ts)} ${x.item} ${x.label ? "clicked" : "not clicked"}: the ASOF JOIN gave it the snapshot of ${fmt.time(x.u_ts)} (${x.u_clicks} clicks); the latest row says ${x.latest_clicks}`));
    svg.append(c);
  }
  return svg;
}

function trainingPage(root, ctx, onState) {
  const { h, card, fmt, table, sqlBlock, toast } = ctx.core;
  const tlHost = h("div"), tlTools = h("div", { class: "row" }), modelsHost = h("div"), aucHost = h("div"), sqlHost = h("div", { class: "fs-sql2" }), wHost = h("div");
  root.append(h("div", { class: "fs-note" }, "A training set needs each impression beside its features as they were just before it: the ", h("b", {}, "ASOF JOIN"), " takes, for each row, the latest snapshot at or before its time. Joining to the latest rows instead lets the label leak in: a clicked impression's own click is in the counters it is joined to. Train one of each and compare: the leaky model looks better offline and is worse on what serving sees."));
  root.append(card({ title: "Two ways to build the training set", body: sqlHost }));
  root.append(h("div", { style: { height: "14px" } }));
  root.append(card({ title: "Point in time, for one shopper", sub: "blue: their clicks in the snapshots; dots: impressions, 8 a request (green: clicked), each at the value the ASOF JOIN gave it; red: the one value a join to the latest rows gives them all", tools: tlTools, body: tlHost }));
  root.append(h("div", { style: { height: "14px" } }));
  root.append(h("div", { class: "grid g2" }, card({ title: "Offline AUC against the truth", sub: "on the last minute, held out", body: aucHost }), card({ title: "Weights", sub: "standardized features, the selected model", body: wHost })));
  root.append(h("div", { style: { height: "14px" } }));
  root.append(card({ title: "Models", sub: "rows of fs_models: weights, the SQL of the training set, timings, AUCs", body: modelsHost, flush: true }));
  root.append(h("div", { style: { height: "14px" } }));
  const wfHost = h("div", { class: "muted" }, "…");
  root.append(card({ title: "Train on the Builder's workflows", sub: "any features, from any workflow", body: wfHost }));
  trainOnWorkflows(ctx, wfHost, (m) => { selected = m.id; loadModels(); });

  let selected = null, models = [], deployed = null;
  const trainBtn = (leaky) => {
    const b = h("button", { class: "btn " + (leaky ? "" : "primary") }, leaky ? "Train on the latest rows (leaks)" : "Train on the ASOF JOIN");
    b.onclick = async () => {
      b.disabled = true; const t = b.textContent; b.textContent = "Training…";
      try { const m = await ctx.api("/api/train", { method: "POST", body: { leaky }, timeout: 300000 }); selected = m.id; toast(`${m.name}: ${fmt.n(m.train_rows)} rows in ${fmt.ms(m.build_ms)}, trained in ${fmt.ms(m.train_ms)}`, "ok"); await loadModels(); }
      catch (e) { toast(String(e.message || e), "bad"); }
      b.disabled = false; b.textContent = t;
    };
    return b;
  };
  ctx.api("/api/models").then((r) => {
    sqlHost.replaceChildren(
      h("div", {}, h("div", { class: "row between" }, h("b", {}, "As of each impression"), h("span", { class: "pill good" }, "point in time")), sqlBlock(r.asof_sql), trainBtn(false)),
      h("div", {}, h("div", { class: "row between" }, h("b", {}, "The latest rows"), h("span", { class: "pill bad" }, "leaks the label")), sqlBlock(r.leaky_sql), trainBtn(true)));
  });

  const aucRow = (label, v, color, note) => [h("span", {}, label), h("div", { class: "bg", title: note || "" }, h("i", { style: { width: `${Math.max(0, (v - 0.5) / 0.5 * 100)}%`, background: color } })), h("span", { class: "num" }, v == null ? "–" : v.toFixed(3))];
  const drawAuc = () => {
    const pick = [];
    const a = models.find((m) => m.join_kind === "asof"), l = models.find((m) => m.join_kind !== "asof");
    if (a) pick.push(a); if (l) pick.push(l);
    if (!pick.length) { aucHost.replaceChildren(h("div", { class: "muted" }, "No model yet: train one of each above.")); return; }
    aucHost.replaceChildren(...pick.map((m) => h("div", { style: { marginBottom: "12px" } },
      h("div", { class: "row", style: { marginBottom: "4px" } }, h("b", {}, m.name), h("span", { class: "pill " + (m.join_kind === "asof" ? "good" : "bad") }, m.join_kind)),
      h("div", { class: "fs-auc" },
        ...aucRow("training rows", m.auc_train, "var(--axis)"),
        ...aucRow("held out, own join", m.auc_holdout, m.join_kind === "asof" ? "var(--s3)" : "var(--s2)", "what this pipeline would report"),
        ...aucRow("held out, as served", m.auc_true, "var(--s1)", "the features serving had at the time: the truth")))),
      h("div", { class: "small muted" }, "AUC from 0.5 (chance) to 1. “As served” scores each model on the last minute with the features it would have had then, from the ASOF JOIN."));
  };
  const drawWeights = () => {
    const m = models.find((x) => x.id === selected) || models[0];
    if (!m) { wHost.replaceChildren(h("div", { class: "muted" }, "–")); return; }
    const max = Math.max(...Object.values(m.weights).map(Math.abs), 0.01);
    wHost.replaceChildren(h("div", { class: "small", style: { marginBottom: "6px" } }, h("b", {}, m.name), ` · ${fmt.n(m.train_rows)} rows`),
      h("div", { class: "fs-auc" }, Object.entries(m.weights).flatMap(([k, v]) => [h("code", {}, k), h("div", { class: "fs-contrib" }, h("div", { class: "axis", style: { gridColumn: "1 / -1" } }, h("i", { style: { left: v >= 0 ? "50%" : `${50 - Math.abs(v) / max * 50}%`, width: `${Math.abs(v) / max * 50}%`, background: v >= 0 ? "var(--s3)" : "var(--s2)" } }))), h("span", { class: "num" }, (v >= 0 ? "+" : "") + v.toFixed(2))])));
  };
  const loadModels = async () => {
    const r = await ctx.api("/api/models");
    models = r.models;
    deployed = models.find((m) => m.deployed)?.id || null;
    modelsHost.replaceChildren(table([
      { key: "name", label: "model", render: (m) => h("b", {}, m.name) },
      { key: "join_kind", label: "training set", render: (m) => h("span", { class: "pill " + (m.join_kind === "asof" ? "good" : "bad") }, m.join_kind) },
      { key: "created", label: "trained", render: (m) => fmt.time(m.created) },
      { key: "train_rows", label: "rows", num: true, render: (m) => fmt.n(m.train_rows) },
      { key: "build_ms", label: "join", num: true, render: (m) => fmt.ms(m.build_ms) },
      { key: "train_ms", label: "fit", num: true, render: (m) => fmt.ms(m.train_ms) },
      { key: "auc_holdout", label: "AUC, own join", num: true, render: (m) => m.auc_holdout?.toFixed(3) },
      { key: "auc_true", label: "AUC, as served", num: true, render: (m) => h("b", {}, m.auc_true?.toFixed(3)) },
      { key: "deployed", label: "", render: (m) => m.deployed ? h("span", { class: "pill ok" }, "serving") : h("button", { class: "btn sm", onclick: async () => { await ctx.api("/api/deploy", { method: "POST", body: { id: m.id } }); toast(`${m.name} serves now`, "ok"); loadModels(); } }, "Deploy") },
    ], { rows: models, compact: true, sortKey: "created", desc: true, onRow: (m) => { selected = m.id; drawWeights(); } }),
    h("div", { class: "row", style: { padding: "8px 12px" } }, h("button", { class: "btn sm", onclick: async () => { await ctx.api("/api/deploy", { method: "POST", body: { id: "random" } }); toast("random ranking serves now", "ok"); loadModels(); } }, "Serve random ranking"), h("span", { class: "small muted" }, "the Pipeline tab charts each model's click rate once it serves")));
    drawAuc(); drawWeights();
  };
  loadModels();

  // The timeline.
  let tuid = "u005";
  const sel = h("select", { class: "select sm" });
  const tlInfo = h("span", { class: "small muted" });
  tlTools.append(tlInfo, sel);
  sel.onchange = () => { tuid = sel.value; drawTl(); };
  ctx.api("/api/users").then((r) => {
    // The most active shoppers first: they have the most to show.
    sel.replaceChildren(...r.users.map((u) => h("option", { value: u.id }, `${u.name} (${u.id})`)));
    sel.value = tuid;
  });
  const drawTl = async () => {
    const r = await ctx.api(`/api/timeline?user=${tuid}`);
    tlInfo.textContent = `${r.impressions.length} impressions, ${r.snaps.length} snapshots · join ${fmt.ms(r.ms)}`;
    tlHost.replaceChildren(timelineSvg(ctx, r));
  };
  const stopTl = every(8000, drawTl);
  return () => stopTl();
}

// ------------------------------------------------------------- speed

function speedPage(root, ctx, onState) {
  const { h, card, fmt, toast } = ctx.core;
  const serveHost = h("div"), trainHost = h("div"), vsHost = h("div"), latHost = h("div");
  root.append(h("div", { class: "grid g2" },
    card({ title: "Serving under load", sub: "each request: 4 reads by key for 489 rows, 7 transforms, the model, the top 8", body: serveHost }),
    card({ title: "Building the training set", sub: "every impression with its features, median of 3", body: trainHost })));
  root.append(h("div", { style: { height: "14px" } }));
  root.append(card({ title: "Latency, live", body: latHost }));
  root.append(h("div", { style: { height: "14px" } }));
  root.append(card({ title: "Against Feast", sub: "measured, not live", body: vsHost }));
  latencyChart(ctx, latHost, onState);

  const runs = [];
  const seg = h("div", { class: "seg" });
  let clients = 4;
  for (const c of [1, 4, 16]) seg.append(h("button", { type: "button", "aria-pressed": String(c === clients), onclick: (e) => { clients = c; for (const b of seg.children) b.setAttribute("aria-pressed", String(b === e.currentTarget)); } }, `${c} client${c > 1 ? "s" : ""}`));
  const go = h("button", { class: "btn primary" }, "Run for 4 s");
  const res = h("div");
  go.onclick = async () => {
    go.disabled = true; go.textContent = "Running…";
    try { runs.unshift(await ctx.api("/api/bench/serve", { method: "POST", body: { clients, seconds: 4 }, timeout: 60000 })); drawRuns(); }
    catch (e) { toast(String(e.message || e), "bad"); }
    go.disabled = false; go.textContent = "Run for 4 s";
  };
  const drawRuns = () => {
    const max = Math.max(1, ...runs.map((r) => r.per_s));
    res.replaceChildren(h("table", { class: "fs-vs" }, h("tr", {}, h("th", {}, "clients"), h("th", { class: "num" }, "requests/s"), h("th", {}, ""), h("th", { class: "num" }, "p50"), h("th", { class: "num" }, "p99")),
      runs.map((r) => h("tr", {}, h("td", {}, r.clients), h("td", { class: "num" }, fmt.n(r.per_s)), h("td", { class: "barcell" }, h("i", { style: { width: `${r.per_s / max * 100}%`, background: "var(--s2)" } })), h("td", { class: "num" }, fmt.ms(r.p50)), h("td", { class: "num" }, fmt.ms(r.p99))))));
  };
  serveHost.append(h("div", { class: "row", style: { marginBottom: "10px" } }, seg, go, h("span", { class: "small muted" }, "the shoppers keep running meanwhile")), res);
  res.append(h("div", { class: "muted small" }, "Runs from this app's process (Python), over HTTP to the server, on this machine."));

  const tgo = h("button", { class: "btn primary" }, "Time it");
  const tres = h("div");
  tgo.onclick = async () => {
    tgo.disabled = true; tgo.textContent = "Joining…";
    try {
      const r = await ctx.api("/api/bench/train", { method: "POST", body: {}, timeout: 300000 });
      tres.replaceChildren(h("table", { class: "fs-vs" }, h("tr", {}, h("th", {}, "join"), h("th", { class: "num" }, "rows"), h("th", { class: "num" }, "time"), h("th", { class: "num" }, "rows/s")),
        [["ASOF JOIN, 3 snapshot tables", r.asof], ["LEFT JOIN to the latest, 4 tables", r.latest]].map(([n, x]) => h("tr", {}, h("td", {}, n), h("td", { class: "num" }, fmt.n(x.rows)), h("td", { class: "num" }, fmt.ms(x.ms)), h("td", { class: "num" }, fmt.si(x.rows / x.ms * 1000))))),
        h("div", { class: "small muted", style: { marginTop: "6px" } }, "Times include sending every row to this app as JSON."));
    } catch (e) { toast(String(e.message || e), "bad"); }
    tgo.disabled = false; tgo.textContent = "Time it";
  };
  trainHost.append(h("div", { class: "row", style: { marginBottom: "10px" } }, tgo), tres);

  ctx.api("/api/measured").then((m) => {
    const parts = [];
    const o = m.offline;
    if (o) {
      const rows = [["pandas merge_asof, in memory", o.pandas_merge_asof_s, "var(--axis)"], ["ChloeDB ASOF JOIN, columnar, over HTTP", o.chloedb_asof_s, "var(--s3)"], ["Feast get_historical_features, DuckDB", o.feast_duckdb_s, "var(--s2)"]];
      const max = Math.max(...rows.map((r) => r[1]));
      parts.push(h("b", {}, `Training set: ${fmt.n(o.spine_rows)} spine rows, features from ${fmt.n(o.user_rows)} and ${fmt.n(o.item_rows)} snapshot rows`),
        h("table", { class: "fs-vs", style: { margin: "6px 0 16px" } }, rows.map(([n, v, c]) => h("tr", {}, h("td", {}, n), h("td", { class: "barcell" }, h("i", { style: { width: `${v / max * 100}%`, background: c } })), h("td", { class: "num" }, `${v.toFixed(2)} s`)))));
    }
    const sv = m.serve;
    if (sv) {
      const cs = [1, 4, 16];
      const max = Math.max(...cs.map((c) => sv[`chloedb_kv@${c}`]?.per_s || 0));
      parts.push(h("b", {}, "Online: 10 features for 1,000 (user, item) pairs a request, over HTTP"),
        h("table", { class: "fs-vs", style: { marginTop: "6px" } }, h("tr", {}, h("th", {}, ""), h("th", { class: "num" }, "clients"), h("th", { class: "num" }, "requests/s"), h("th", {}, ""), h("th", { class: "num" }, "p50")),
          cs.flatMap((c) => [["ChloeDB /v1/kv/get", sv[`chloedb_kv@${c}`], "var(--s3)"], ["Feast feature server, Redis", sv[`feast_server_redis@${c}`], "var(--s2)"]].map(([n, x, col]) => x ? h("tr", {}, h("td", {}, n), h("td", { class: "num" }, c), h("td", { class: "num" }, fmt.n(x.per_s)), h("td", { class: "barcell" }, h("i", { style: { width: `${x.per_s / max * 100}%`, background: col } })), h("td", { class: "num" }, fmt.ms(x.p50_ms))) : null))));
    }
    parts.push(h("div", { class: "small muted", style: { marginTop: "8px" } }, `From ${m.source}. Same data and values on both sides.`));
    vsHost.replaceChildren(...(parts.length > 1 ? parts : [h("div", { class: "muted" }, "The measured results are not in this checkout.")]));
  });
  return () => {};
}

// ------------------------------------------------------------- builder

const BCSS = `
.fb-map { width: 100%; height: auto; display: block; }
.fb-map .n rect { fill: var(--surface); stroke: var(--line-2); stroke-width: 1.2; cursor: pointer; }
.fb-map .n.table rect { fill: var(--surface-2); }
.fb-map .n.workflow rect { stroke: var(--s1); }
.fb-map .n.model rect { stroke: var(--s3); }
.fb-map .n.model.live rect { stroke-width: 2.4; }
.fb-map .n.sel rect { stroke: var(--accent); stroke-width: 2.6; }
.fb-map .n.hit rect { stroke: var(--crit); stroke-width: 2.6; fill: var(--crit-soft); }
.fb-map .n.down rect { stroke: var(--s2); stroke-width: 2; fill: var(--serious-soft); }
.fb-map .n.up rect { stroke: var(--s4); stroke-width: 2; }
.fb-map.searching .n:not(.hit):not(.down):not(.up) { opacity: .35; }
.fb-map.searching .e:not(.lit) { opacity: .2; }
.fb-map .t { font-size: 12.5px; font-weight: 650; fill: var(--text); pointer-events: none; }
.fb-map .k { font-size: 9.5px; fill: var(--muted); text-transform: uppercase; letter-spacing: .05em; pointer-events: none; }
.fb-map .m { font-size: 10.5px; fill: var(--text-2); font-family: var(--mono); pointer-events: none; }
.fb-map .e { fill: none; stroke: var(--axis); stroke-width: 1.5; }
.fb-map .e.join { stroke-dasharray: 5 4; }
.fb-map .e.lit { stroke: var(--crit); stroke-width: 2; }
.fb-map .fl { fill: none; stroke: var(--s1); stroke-width: 2.4; stroke-dasharray: 3 11; stroke-linecap: round; animation: fs-flow 1s linear infinite; opacity: 0; transition: opacity .4s; }
.fb-map .fl.on { opacity: 1; }
.fb-map .bad { fill: var(--crit); }
.fb-search { display: grid; grid-template-columns: minmax(0, 1fr) auto auto; gap: 8px; margin-bottom: 8px; }
.fb-search .input { font-family: var(--mono); font-size: 12.5px; }
.fb-ex { display: flex; gap: 6px; flex-wrap: wrap; margin-bottom: 10px; font-size: 12px; }
.fb-ex button { font-family: var(--mono); }
.fb-two { display: grid; grid-template-columns: minmax(0, 1.25fr) minmax(0, 1fr); gap: 14px; align-items: start; }
.fb-ed .lab { font-size: 11.5px; color: var(--muted); font-weight: 600; text-transform: uppercase; letter-spacing: .04em; margin: 12px 0 4px; }
.fb-ed .lab:first-child { margin-top: 0; }
.fb-ed .input, .fb-ed .select { font-family: var(--mono); font-size: 12.5px; }
.fb-row { display: grid; gap: 6px; margin-bottom: 6px; align-items: center; }
.fb-in { grid-template-columns: 26px minmax(0, 1.2fr) 60px minmax(0, 1fr) minmax(0, .8fr) 28px; }
.fb-ag { grid-template-columns: minmax(0, 1fr) 128px minmax(0, 1.2fr) 92px minmax(0, 1fr) 28px; }
.fb-ex2 { grid-template-columns: minmax(0, 1fr) minmax(0, 2.6fr) 28px; }
.fb-chips { display: flex; flex-wrap: wrap; gap: 4px; margin: 4px 0 2px; }
.fb-chips button { font-family: var(--mono); font-size: 11px; padding: 1px 6px; border-radius: 5px; border: 1px solid var(--line); background: var(--surface-2); cursor: pointer; color: var(--text-2); }
.fb-chips button:hover { border-color: var(--accent); color: var(--text); }
.fb-stats td.bar { width: 90px; }
.fb-stats td.bar i { display: block; height: 8px; border-radius: 3px; background: var(--crit); }
.fb-stats tr.hit td { background: var(--crit-soft); }
.fb-pre { font-size: 11.5px; white-space: pre-wrap; max-height: 300px; overflow: auto; margin: 6px 0; }
.fb-tw { display: grid; grid-template-columns: repeat(auto-fill, minmax(250px, 1fr)); gap: 10px; }
.fb-tw > div { border: 1px solid var(--line); border-radius: var(--r-sm); padding: 8px 10px; }
.fb-tw label { display: flex; gap: 6px; align-items: center; font-size: 12.5px; font-family: var(--mono); }
@media (max-width: 1100px) { .fb-two { grid-template-columns: minmax(0, 1fr); } }
@media (max-width: 700px) { .fb-in, .fb-ag, .fb-ex2 { grid-template-columns: minmax(0, 1fr); } }
`;

/** Layers left to right: tables, then each workflow after its inputs, then models. */
function layout(g) {
  const byId = new Map(g.nodes.map((n) => [n.id, { ...n }]));
  const parents = new Map();
  for (const e of g.edges) { if (!parents.has(e.to)) parents.set(e.to, []); parents.get(e.to).push(e.from); }
  const layer = (id, seen = new Set()) => {
    const n = byId.get(id);
    if (!n) return 0;
    if (n.layer != null) return n.layer;
    if (seen.has(id)) return 0;
    seen.add(id);
    const ps = parents.get(id) || [];
    n.layer = n.kind === "table" ? 0 : 1 + Math.max(0, ...ps.map((p) => layer(p, seen)));
    return n.layer;
  };
  for (const id of byId.keys()) layer(id);
  const maxL = Math.max(0, ...[...byId.values()].filter((n) => n.kind !== "model").map((n) => n.layer));
  for (const n of byId.values()) if (n.kind === "model") n.layer = maxL + 1;
  const cols = [];
  for (const n of byId.values()) (cols[n.layer] ||= []).push(n);
  const CW = 236, RH = 96;
  const NW = { table: 180, workflow: 200, model: 170 }, NH = { table: 52, workflow: 74, model: 52 };
  let height = 0;
  cols.forEach((col, L) => {
    if (!col) return;
    // Order by the parents' places, to keep the lines short.
    col.sort((a, b) => {
      const ya = (parents.get(a.id) || []).map((p) => byId.get(p)?.y ?? 0), yb = (parents.get(b.id) || []).map((p) => byId.get(p)?.y ?? 0);
      const ma = ya.length ? ya.reduce((s, v) => s + v, 0) / ya.length : 0, mb = yb.length ? yb.reduce((s, v) => s + v, 0) / yb.length : 0;
      return ma - mb || a.name.localeCompare(b.name);
    });
    col.forEach((n, i) => { n.x = 16 + L * CW; n.y = 18 + i * RH; n.w = NW[n.kind]; n.h = NH[n.kind]; });
    height = Math.max(height, 18 + col.length * RH);
  });
  return { nodes: byId, width: 16 + cols.length * CW, height: Math.max(height, 140), parents };
}

function builderPage(root, ctx) {
  const { h, s, card, fmt, toast, sqlBlock, table } = ctx.core;
  root.append(h("style", {}, BCSS));
  const intro = h("div", { class: "fs-note" }, "Workflows turn tables into features: each reads one or more inputs (tables, or other workflows' output), groups by an entity key, aggregates over time windows and transforms. It writes ",
    h("code", {}, "fv_<name>"), ", served by key, and ", h("code", {}, "fv_<name>_hist"), ", which training sets join as of each event. Every run records each feature's statistics in ", h("code", {}, "fs_workflow_stats"), "; search them below.");
  const where = h("input", { class: "input", placeholder: "search the features' statistics: null_share > 0.2, rows < 100, max > 1000, age_s > 30 …" });
  const go = h("button", { class: "btn primary" }, "Search");
  const clear = h("button", { class: "btn" }, "Clear");
  const found = h("div", { class: "small muted" });
  const ex = h("div", { class: "fb-ex" }, h("span", { class: "muted" }, "try:"),
    ...["zeros > rows / 3", "distinct_values < 20", "max > 1000", "null_share > 0", "run_ms > 150", "age_s > 20"].map((q) => h("button", { class: "btn xs", onclick: () => { where.value = q; search(); } }, q)));
  const mapHost = h("div", { style: { overflowX: "auto" } });
  const newBtn = h("button", { class: "btn sm primary", onclick: () => edit(null) }, "New workflow");
  const detail = h("div"), editor = h("div");
  const closeEditor = () => { editor.replaceChildren(); detail.hidden = false; };
  root.append(intro, card({ title: "The map", sub: "tables → workflows → models; dashed: a joined input", tools: [newBtn], body: [h("div", { class: "fb-search" }, where, go, clear), ex, found, mapHost] }));
  root.append(h("div", { style: { height: "14px" } }), editor, detail);

  let g = null, L = null, sel = null, hits = null, tables = null;
  const svgNodes = new Map(), flows = new Map(), edgeEls = [];

  const draw = () => {
    L = layout(g);
    svgNodes.clear(); flows.clear(); edgeEls.length = 0;
    const svg = s("svg", { class: "fb-map", viewBox: `0 0 ${L.width} ${L.height}`, style: `width: ${L.width}px; max-width: 100%` });
    for (const e of g.edges) {
      const a = L.nodes.get(e.from), b = L.nodes.get(e.to);
      if (!a || !b) continue;
      const x0 = a.x + a.w, y0 = a.y + a.h / 2, x1 = b.x, y1 = b.y + b.h / 2, mx = (x0 + x1) / 2;
      const d = `M${x0},${y0} C${mx},${y0} ${mx},${y1} ${x1},${y1}`;
      const p = s("path", { class: "e" + (e.primary ? "" : " join"), d });
      if (e.on) p.append(s("title", {}, `joined on ${e.on[0]} = ${e.on[1]}`));
      edgeEls.push([e, p]);
      const f = s("path", { class: "fl", d });
      flows.set(e.to + "|" + e.from, [b, f]);
      svg.append(p, f);
    }
    for (const n of L.nodes.values()) {
      const gN = s("g", { class: `n ${n.kind}` + (n.deployed ? " live" : "") });
      gN.append(s("rect", { x: n.x, y: n.y, width: n.w, height: n.h, rx: 8 }));
      if (n.kind === "table") {
        gN.append(s("text", { class: "k", x: n.x + 10, y: n.y + 15 }, "table"), s("text", { class: "t", x: n.x + 10, y: n.y + 31 }, n.name), s("text", { class: "m", x: n.x + 10, y: n.y + 45 }, `${fmt.si(n.rows)} rows`));
      } else if (n.kind === "workflow") {
        const last = n.last;
        gN.append(s("text", { class: "k", x: n.x + 10, y: n.y + 15 }, `${n.mode} · every ${n.every} s`), s("text", { class: "t", x: n.x + 10, y: n.y + 31 }, n.name),
          s("text", { class: "m", x: n.x + 10, y: n.y + 46 }, `→ ${n.target}`),
          s("text", { class: "m", x: n.x + 10, y: n.y + 62 }, last ? `${fmt.ms(last.ms)} · ${fmt.si(last.rows)} rows · ${fmt.si(last.changed)} new` : "not run"));
        if (n.errors) gN.append(s("circle", { class: "bad", cx: n.x + n.w - 12, cy: n.y + 12, r: 5 }));
      } else {
        gN.append(s("text", { class: "k", x: n.x + 10, y: n.y + 15 }, n.deployed ? "model · serving" : "model"), s("text", { class: "t", x: n.x + 10, y: n.y + 31 }, n.name),
          s("text", { class: "m", x: n.x + 10, y: n.y + 45 }, `AUC ${n.auc?.toFixed(3) ?? "–"}`));
      }
      gN.onclick = () => { sel = n.id; mark(); closeEditor(); showDetail(); };
      svgNodes.set(n.id, gN);
      svg.append(gN);
    }
    mapHost.replaceChildren(svg);
    mark();
  };
  const mark = () => {
    if (!L) return;
    const svg = mapHost.firstChild;
    svg?.classList.toggle("searching", !!hits);
    const hw = new Set((hits?.workflows || []).map((w) => "w:" + w)), dw = new Set((hits?.downstream || []).map((w) => "w:" + w)), uw = new Set((hits?.upstream || []).map((w) => "w:" + w));
    // Models fed by a hit or what it feeds are downstream too.
    for (const e of g.edges) if (e.to.startsWith("m:") && (hw.has(e.from) || dw.has(e.from))) dw.add(e.to);
    for (const [id, el] of svgNodes) {
      el.classList.toggle("sel", id === sel);
      el.classList.toggle("hit", hw.has(id)); el.classList.toggle("down", dw.has(id)); el.classList.toggle("up", uw.has(id));
    }
    for (const [e, p] of edgeEls) p.classList.toggle("lit", !!hits && (hw.has(e.from) || dw.has(e.from)) && (dw.has(e.to) || hw.has(e.to)));
  };
  const pulse = () => {
    if (!g) return;
    for (const [, [b, f]] of flows) {
      const n = g.nodes.find((x) => x.id === b.id);
      f.classList.toggle("on", !!(n && n.kind === "workflow" && n.age_ms != null && n.age_ms < 2500));
    }
  };
  const load = async (redraw) => {
    const ng = await ctx.api("/api/wf/graph");
    const shape = (x) => JSON.stringify([x.nodes.map((n) => n.id).sort(), x.edges.map((e) => e.from + e.to).sort()]);
    const changed = !g || shape(g) !== shape(ng);
    g = ng;
    if (changed || redraw) draw();
    else {
      // Update the numbers in place.
      for (const n of g.nodes) {
        const el = svgNodes.get(n.id);
        if (!el) continue;
        const ms = el.querySelectorAll(".m");
        if (n.kind === "workflow" && ms[1]) ms[1].textContent = n.last ? `${fmt.ms(n.last.ms)} · ${fmt.si(n.last.rows)} rows · ${fmt.si(n.last.changed)} new` : "not run";
        if (n.kind === "table" && ms[0]) ms[0].textContent = `${fmt.si(n.rows)} rows`;
      }
      pulse();
    }
    found.textContent = g.status !== "live" ? `Workflows: ${g.status}…` : found.textContent;
  };

  async function search() {
    const q = where.value.trim();
    if (!q) { hits = null; found.textContent = ""; mark(); showDetail(); return; }
    try {
      hits = await ctx.api("/api/wf/search", { method: "POST", body: { where: q } });
      found.replaceChildren(hits.features.length
        ? h("span", {}, h("b", { style: { color: "var(--crit)" } }, `${hits.features.length} feature${hits.features.length > 1 ? "s" : ""} in ${hits.workflows.length} workflow${hits.workflows.length > 1 ? "s" : ""}`),
          `: ${hits.features.slice(0, 8).map((f) => `${f.wf}.${f.feature}`).join(", ")}${hits.features.length > 8 ? ", …" : ""}. `,
          hits.downstream.length ? h("span", {}, "They feed ", h("b", { style: { color: "var(--s2)" } }, hits.downstream.join(", ")), " and the models after them. ") : null,
          hits.upstream.length ? h("span", {}, "They read ", h("b", { style: { color: "var(--s4)" } }, hits.upstream.join(", ")), ".") : null)
        : "No feature matches.");
      if (hits.workflows.length && !hits.workflows.includes((sel || "").slice(2))) sel = "w:" + hits.workflows[0];
      mark(); showDetail();
    } catch (e) { found.textContent = String(e.message || e); }
  }
  go.onclick = search;
  where.onkeydown = (e) => { if (e.key === "Enter") search(); };
  clear.onclick = () => { where.value = ""; search(); };

  async function showDetail() {
    if (!sel) { detail.replaceChildren(); return; }
    const n = g.nodes.find((x) => x.id === sel);
    if (!n) { detail.replaceChildren(); return; }
    if (n.kind === "table") {
      tables = tables || await ctx.api("/api/wf/tables");
      const t = tables.tables.find((x) => x.table === n.name);
      detail.replaceChildren(card({ title: n.name, sub: `a table · ${fmt.n(n.rows)} rows`, body: h("div", { class: "fb-chips" }, (t?.columns || []).map((c) => h("button", {}, `${c.name} ${c.type}`))) }));
      return;
    }
    if (n.kind === "model") {
      detail.replaceChildren(card({ title: n.name, sub: n.deployed ? "serving the shop" : "a model", body: h("div", {}, `AUC as served ${n.auc?.toFixed(3)}. `, h("a", { href: ctx.href("training") }, "Training"), " lists it, with its weights and the SQL of its training set.") }));
      return;
    }
    const d = await ctx.api(`/api/wf/spec?name=${encodeURIComponent(n.name)}`);
    const hitF = new Set((hits?.features || []).filter((f) => f.wf === n.name).map((f) => f.feature));
    const runs = d.runs.filter((r) => !r.backfill);
    const runChart = ctx.charts.xyChart({ height: 150, yfmt: (v) => fmt.ms(v), xfmt: (x) => fmt.time(x), min: 0,
      series: [{ name: "run time", color: "var(--s1)", points: runs.map((r) => [r.t, r.ms]) }, { name: "query", color: "var(--s4)", points: runs.map((r) => [r.t, r.query_ms]) }] });
    const keyIn = h("input", { class: "input sm", placeholder: "an entity key, e.g. " + (n.name.includes("user_category") ? "u005|sport" : n.name.startsWith("user") ? "u005" : "p010") });
    const look = h("div", { class: "small" });
    keyIn.onkeydown = async (e) => {
      if (e.key !== "Enter") return;
      const r = await ctx.api("/api/wf/lookup", { method: "POST", body: { name: n.name, keys: [keyIn.value.trim()] } });
      look.replaceChildren(h("code", {}, JSON.stringify(r.rows[0])), h("span", { class: "muted" }, `  · POST /v1/kv/get on ${r.table}: ${fmt.ms(r.ms)}`));
    };
    const statsRows = d.spec.features.map((f) => ({ name: f.name, def: f.expr ? f.expr : `${f.fn}(${f.arg || "*"})${f.window ? ` over ${f.window} s` : ""}${f.where ? ` where ${f.where}` : ""}`, ...(n.stats[f.name] || {}) }));
    const act = (label, f, cls = "btn sm") => { const b = h("button", { class: cls }, label); b.onclick = async () => { b.disabled = true; try { await f(); } catch (e) { toast(String(e.message || e), "bad"); } b.disabled = false; }; return b; };
    detail.replaceChildren(card({ title: n.name, sub: `${d.spec.title || ""} → ${d.target}, every ${d.spec.every} s`,
      tools: [act("Edit", () => edit(d.spec)), act("Run now", async () => { const r = await ctx.api("/api/wf/run", { method: "POST", body: { name: n.name } }); toast(`${n.name}: ${fmt.ms(r.ms)}, ${r.rows} rows`, r.error ? "bad" : "ok"); load(); }),
        act("Backfill 10 min", async () => { await ctx.api("/api/wf/backfill", { method: "POST", body: { name: n.name, minutes: 10 }, timeout: 300000 }); toast(`${n.name}: history for the last 10 minutes`, "ok"); }),
        act("Delete", async () => { await ctx.api("/api/wf/delete", { method: "POST", body: { name: n.name } }); sel = null; toast(`${n.name} deleted`, "ok"); load(true); detail.replaceChildren(); }, "btn sm danger")],
      body: [
        h("div", { class: "small muted", style: { marginBottom: "6px" } }, `reads ${d.upstream.length ? d.upstream.join(", ") + " and " : ""}${d.spec.inputs.filter((i) => i.table).map((i) => i.table).join(", ") || "only workflows"}; ${d.downstream.length ? "feeds " + d.downstream.join(", ") : "feeds no workflow"}`),
        h("div", { class: "fb-two" },
          h("div", {}, table([
            { key: "name", label: "feature", render: (r) => h("code", {}, r.name) },
            { key: "def", label: "definition", render: (r) => h("span", { class: "small" }, r.def) },
            { key: "null_share", label: "missing", num: true, render: (r) => r.null_share == null ? "–" : h("span", {}, fmt.pct(r.null_share)) },
            { key: "min", label: "min", num: true, render: (r) => r.min == null ? "–" : fmt.si(r.min, 3) },
            { key: "mean", label: "mean", num: true, render: (r) => r.mean == null ? "–" : fmt.si(r.mean, 3) },
            { key: "max", label: "max", num: true, render: (r) => r.max == null ? "–" : fmt.si(r.max, 3) },
            { key: "distinct_values", label: "distinct", num: true },
          ], { rows: statsRows, compact: true, rowClass: (r) => hitF.has(r.name) ? "hit" : "" }),
          h("div", { class: "row", style: { marginTop: "10px" } }, h("span", { class: "small muted" }, "Serve:"), keyIn), look),
          h("div", {}, runChart, h("div", { class: "lab small muted", style: { marginTop: "8px" } }, "The SQL of a run (:now is the run's time)"), sqlBlock(d.sql, { class: "code fb-pre" })))] }));
    runChart.update?.([{ name: "run time", color: "var(--s1)", points: runs.map((r) => [r.t, r.ms]) }, { name: "query", color: "var(--s4)", points: runs.map((r) => [r.t, r.query_ms]) }]);
    detail.classList.add("fs-flash"); setTimeout(() => detail.classList.remove("fs-flash"), 1600);
  }

  // ------------------------------------------------------------- editor
  async function edit(spec) {
    detail.hidden = true;
    tables = await ctx.api("/api/wf/tables");
    const sp = JSON.parse(JSON.stringify(spec || { name: "", title: "", inputs: [{ table: "fs_impressions", alias: "a" }], mode: "aggregate", key: "a.usr", time: "a.ts", where: "", features: [{ name: "views", fn: "count" }], every: 10 }));
    const isNew = !spec;
    let focused = null;
    const inp = (v, on, ph, cls = "input sm") => { const e = h("input", { class: cls, value: v ?? "", placeholder: ph || "" }); e.oninput = () => { on(e.value); changed(); }; e.onfocus = () => { focused = e; }; return e; };
    const sel_ = (opts, v, on, cls = "select sm") => { const e = h("select", { class: cls }, opts.map((o) => h("option", { value: o[0] }, o[1]))); e.value = v ?? ""; e.onchange = () => { on(e.value); changed(); }; return e; };
    const colsOf = (i) => i.workflow ? ["entity", "ts", ...(tables.workflows[i.workflow]?.features || [])] : (tables.tables.find((t) => t.table === i.table)?.columns || []).map((c) => c.name);
    const sources = [...tables.tables.filter((t) => !t.table.startsWith("fv_") && !t.table.startsWith("fs_workflow")).map((t) => ["t:" + t.table, t.table]), ...Object.keys(tables.workflows).filter((w) => w !== sp.name).map((w) => ["w:" + w, `workflow ${w}`])];
    const body = h("div", { class: "fb-ed" });
    const out = h("div");
    const insert = (txt) => { if (!focused) return; const st = focused.selectionStart ?? focused.value.length; focused.value = focused.value.slice(0, st) + txt + focused.value.slice(focused.selectionEnd ?? st); focused.dispatchEvent(new Event("input")); focused.focus(); };
    const render = () => {
      const ins = h("div");
      sp.inputs.forEach((i, k) => {
        const src = sel_(sources, i.workflow ? "w:" + i.workflow : "t:" + i.table, (v) => { delete i.table; delete i.workflow; if (v.startsWith("w:")) i.workflow = v.slice(2); else i.table = v.slice(2); render(); });
        const own = colsOf(i);
        ins.append(h("div", { class: "fb-row fb-in" }, h("span", { class: "small muted" }, k ? "join" : "from"), src, inp(i.alias, (v) => { i.alias = v; }, "alias"),
          k ? inp(i.on?.[0], (v) => { i.on = [v, i.on?.[1] || ""]; }, "a.item  (from the inputs before)") : h("span", { class: "small muted" }, "grouped by the key below"),
          k ? sel_([["", "= its column…"], ...own.map((c) => [c, "= " + i.alias + "." + c])], i.on?.[1], (v) => { i.on = [i.on?.[0] || "", v]; }) : h("span"),
          k ? h("button", { class: "btn xs ghost", title: "remove", onclick: () => { sp.inputs.splice(k, 1); render(); changed(); } }, "✕") : h("span")),
          h("div", { class: "fb-chips" }, own.map((c) => h("button", { type: "button", title: "insert", onclick: () => insert(`${i.alias}.${c}`) }, `${i.alias}.${c}`))));
      });
      const aggs = h("div"), exprs = h("div");
      sp.features.forEach((f, k) => {
        const rm = h("button", { class: "btn xs ghost", title: "remove", onclick: () => { sp.features.splice(k, 1); render(); changed(); } }, "✕");
        if (f.expr != null) exprs.append(h("div", { class: "fb-row fb-ex2" }, inp(f.name, (v) => { f.name = v; }, "name"), inp(f.expr, (v) => { f.expr = v; }, "(clicks + 1.0) / (views + 10), clamp(x, 0, 1), ln(1 + x) …"), rm));
        else aggs.append(h("div", { class: "fb-row fb-ag" }, inp(f.name, (v) => { f.name = v; }, "name"), sel_(tables.fns.map((x) => [x, x]), f.fn, (v) => { f.fn = v; }),
          inp(f.arg, (v) => { f.arg = v; }, f.fn === "count" ? "*" : "a.clicked"), sel_([["", "all time"], ...tables.windows.map((w) => [String(w), w < 3600 ? `last ${w / 60} min` : w < 86400 ? `last ${w / 3600} h` : "last day"])], f.window ? String(f.window) : "", (v) => { f.window = v ? Number(v) : null; }),
          inp(f.where, (v) => { f.where = v; }, "only where …"), rm));
      });
      body.replaceChildren(
        h("div", { class: "lab" }, "Name and title"), h("div", { class: "fb-row", style: { gridTemplateColumns: "minmax(0,1fr) minmax(0,2fr) 120px" } },
          isNew ? inp(sp.name, (v) => { sp.name = v; }, "user_recent") : h("code", {}, sp.name), inp(sp.title, (v) => { sp.title = v; }, "what it is for"),
          sel_([["5", "every 5 s"], ["10", "every 10 s"], ["30", "every 30 s"], ["60", "every minute"], ["300", "every 5 min"]], String(sp.every), (v) => { sp.every = Number(v); })),
        h("div", { class: "lab" }, "Inputs: tables, or other workflows (read as of each run)"), ins,
        h("button", { class: "btn xs", onclick: () => { sp.inputs.push({ table: "fs_items", alias: "abcdefgh"[sp.inputs.length] || "x", on: ["", ""] }); render(); } }, "+ join an input"),
        h("div", { class: "lab" }, "Rows"), h("div", { class: "fb-row", style: { gridTemplateColumns: "150px minmax(0,1fr) minmax(0,1fr)" } },
          sel_([["aggregate", "grouped by key"], ["map", "one per input row"]], sp.mode, (v) => { sp.mode = v; if (v === "map") sp.features = sp.features.filter((f) => f.expr != null); render(); }),
          inp(sp.key, (v) => { sp.key = v; }, "key: a.usr, a.usr || '|' || p.category"), inp(sp.time, (v) => { sp.time = v; }, "time (epoch ms): a.ts")),
        inp(sp.where, (v) => { sp.where = v; }, "WHERE … (optional)", "input sm"),
        sp.mode === "aggregate" ? h("div", {}, h("div", { class: "lab" }, "Aggregates"), aggs, h("button", { class: "btn xs", onclick: () => { sp.features.push({ name: "", fn: "count" }); render(); } }, "+ aggregate")) : "",
        h("div", { class: "lab" }, sp.mode === "aggregate" ? "Transformations, over the aggregates" : "Transformations, over the inputs' columns"), exprs,
        h("button", { class: "btn xs", onclick: () => { sp.features.push({ name: "", expr: "" }); render(); } }, "+ transformation"));
    };
    const backfill = h("select", { class: "select sm" }, h("option", { value: "0" }, "no history"), h("option", { value: "10" }, "backfill 10 min"), h("option", { value: "30" }, "backfill 30 min"));
    backfill.value = "10";
    const save = h("button", { class: "btn primary" }, isNew ? "Create" : "Save");
    save.onclick = async () => {
      save.disabled = true; save.textContent = "Saving…";
      try {
        const r = await ctx.api("/api/wf/save", { method: "POST", body: { spec: clean(), backfill: Number(backfill.value) }, timeout: 600000 });
        toast(`${r.saved} saved: ${r.run ? `${fmt.ms(r.run.ms)}, ${r.run.rows} rows` : ""}`, "ok");
        sel = "w:" + r.saved; editor.replaceChildren(); detail.hidden = false; await load(true); showDetail();
      } catch (e) { toast(String(e.message || e), "bad"); }
      save.disabled = false; save.textContent = isNew ? "Create" : "Save";
    };
    const clean = () => {
      const c = JSON.parse(JSON.stringify(sp));
      for (const f of c.features) { for (const k of ["arg", "where"]) if (!f[k]) delete f[k]; if (!f.window) delete f.window; if (f.expr != null) { delete f.fn; delete f.window; } }
      return c;
    };
    let timer = null, seq = 0;
    const changed = () => {
      clearTimeout(timer);
      timer = setTimeout(async () => {
        const my = ++seq;
        const c = clean();
        if (!c.name) c.name = "preview";
        try {
          const r = await ctx.api("/api/wf/preview", { method: "POST", body: { spec: c } });
          if (my !== seq) return;
          out.replaceChildren(h("div", { class: "row between small" }, h("b", {}, `${fmt.n(r.count)} rows`), h("span", { class: "muted" }, `the query took ${fmt.ms(r.ms)}`)),
            sqlBlock(r.sql, { class: "code fb-pre" }),
            table(r.columns.map((col) => ({ key: col, label: col, num: typeof r.rows[0]?.[col] === "number", render: (x) => x[col] == null ? h("span", { style: { color: "var(--crit)" } }, "null") : typeof x[col] === "number" ? fmt.si(x[col], 3) : String(x[col]) })), { rows: r.rows, compact: true, maxHeight: "260px" }),
            h("div", { class: "small muted", style: { marginTop: "6px" } }, Object.entries(r.stats).map(([k, v]) => `${k}: ${fmt.pct(v.null_share)} missing`).join(" · ")));
        } catch (e) { if (my === seq) out.replaceChildren(h("div", { class: "fs-note warn" }, String(e.message || e))); }
      }, 500);
    };
    render();
    editor.replaceChildren(card({ title: isNew ? "New workflow" : `Edit ${sp.name}`, sub: "the preview runs as you type", tools: [backfill, save, h("button", { class: "btn sm ghost", onclick: () => { editor.replaceChildren(); detail.hidden = false; } }, "Close")],
      body: h("div", { class: "fb-two" }, body, h("div", {}, h("div", { class: "lab small muted" }, "Preview: what the next run writes"), out)) }), h("div", { style: { height: "14px" } }));
    changed();
    editor.scrollIntoView({ behavior: "smooth", block: "start" });
  }

  load(true).then(() => { if (!sel) { sel = g.nodes.find((n) => n.kind === "workflow") ? "w:" + g.nodes.find((n) => n.kind === "workflow").name : null; mark(); showDetail(); } });
  const t = setInterval(() => load().catch(() => {}), 2000);
  return () => clearInterval(t);
}

/** In the Training tab: a model over the workflows' features. */
function trainOnWorkflows(ctx, host, onTrained) {
  const { h, fmt, toast, sqlBlock } = ctx.core;
  const ON = ["usr", "item", "uc", "category"];
  const guess = (name, specs) => {
    const s = specs[name];
    if (!s) return "usr";
    const k = s.key || "";
    for (const c of ON) if (k.endsWith("." + c)) return c;
    const up = s.inputs?.[0]?.workflow;
    return up ? guess(up, specs) : "usr";
  };
  ctx.api("/api/wf/graph").then(async (g) => {
    const specs = {};
    for (const n of g.nodes.filter((x) => x.kind === "workflow")) specs[n.name] = (await ctx.api(`/api/wf/spec?name=${encodeURIComponent(n.name)}`)).spec;
    const pick = {};
    const boxes = h("div", { class: "fb-tw" });
    for (const n of g.nodes.filter((x) => x.kind === "workflow")) {
      const numeric = n.features.filter((f) => n.stats[f] && n.stats[f].min != null);
      pick[n.name] = { on: guess(n.name, specs), feats: new Set() };
      const on = h("select", { class: "select sm" }, ON.map((c) => h("option", { value: c }, `on ${c}`)));
      on.value = pick[n.name].on; on.onchange = () => { pick[n.name].on = on.value; };
      boxes.append(h("div", {}, h("div", { class: "row between", style: { marginBottom: "4px" } }, h("b", {}, n.name), on),
        ...numeric.map((f) => { const c = h("input", { type: "checkbox" }); c.onchange = () => { if (c.checked) pick[n.name].feats.add(f); else pick[n.name].feats.delete(f); }; return h("label", {}, c, f); })));
    }
    const shop = {};
    boxes.append(h("div", {}, h("b", {}, "the impression"), ...["price", "pos"].map((f) => { const c = h("input", { type: "checkbox" }); c.onchange = () => { shop[f] = c.checked; }; return h("label", {}, c, f); })));
    // A good start: one or two features of each.
    const preset = { user_activity: ["ctr"], item_popularity: ["ctr", "trend_ctr"], item_signals: ["hot", "log_views"], user_category_share: ["affinity", "share", "lift"] };
    for (const [w, fs] of Object.entries(preset)) for (const f of fs) { pick[w]?.feats.add(f); }
    for (const lab of boxes.querySelectorAll("label")) { const w = lab.closest("div").querySelector("b").textContent; if (pick[w]?.feats.has(lab.textContent)) lab.querySelector("input").checked = true; }
    const body = () => {
      const joins = [], features = [];
      for (const [w, p] of Object.entries(pick)) if (p.feats.size) { joins.push({ workflow: w, on: p.on }); for (const f of p.feats) features.push(`${w}.${f}`); }
      for (const [f, v] of Object.entries(shop)) if (v) features.push(`shop.${f}`);
      return { joins, features };
    };
    const sqlOut = h("div");
    const show = h("button", { class: "btn sm" }, "Show the SQL");
    show.onclick = async () => { try { const r = await ctx.api("/api/wf/training_sql", { method: "POST", body: body() }); sqlOut.replaceChildren(sqlBlock(r.sql, { class: "code fb-pre" })); } catch (e) { toast(String(e.message || e), "bad"); } };
    const train = (leaky) => { const b = h("button", { class: "btn sm " + (leaky ? "" : "primary") }, leaky ? "Train on the latest rows (leaks)" : "Train as of each impression"); b.onclick = async () => {
      b.disabled = true; const t = b.textContent; b.textContent = "Training…";
      try { const m = await ctx.api("/api/wf/train", { method: "POST", body: { ...body(), leaky }, timeout: 600000 }); toast(`${m.name}: ${fmt.n(m.train_rows)} rows, AUC as served ${m.auc_true?.toFixed(3)}`, "ok"); onTrained(m); }
      catch (e) { toast(String(e.message || e), "bad"); }
      b.disabled = false; b.textContent = t; }; return b; };
    host.replaceChildren(h("div", { class: "small muted", style: { marginBottom: "8px" } }, "Pick features from any workflow in the Builder, and the impression column each joins on. The training set joins each workflow's history as of each impression; serving reads each workflow's table by key."),
      boxes, h("div", { class: "row", style: { marginTop: "10px" } }, train(false), train(true), show), sqlOut);
  });
}
