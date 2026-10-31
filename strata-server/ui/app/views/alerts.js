// Alerts: the rules the server checks on every point, their state now,
// a timeline of when each fired, and an editor where the threshold is a
// line to drag on the metric's own chart (it shows how often the rule
// would have fired over the range). Rules reorder by dragging.

import { h, s, icon, clear, fmt, card, api, bus, drawer, toast, confirm, pill, seg, toggle, empty, sortable, showTip, hideTip, ApiError } from "../core.js";
import { timeChart, sparkline, SERIES } from "../charts.js";
import { state, domain, visible, refreshAlerts } from "../data.js";

export default { id: "alerts", title: "Alerts", icon: "alerts", group: "Monitor", keywords: "rules thresholds webhook firing incidents notifications", render, onParams };

const OPS = [">", ">=", "<", "<="];
const SEV = ["critical", "warning", "info"];
const rank = { firing: 0, pending: 1, "no data": 2, ok: 3, disabled: 4 };
const test = (v, op, t) => v != null && (op === ">" ? v > t : op === ">=" ? v >= t : op === "<" ? v < t : v <= t);

function metricsOfPoints() {
  const keys = new Set();
  for (const p of state.points.slice(-5)) for (const [k, v] of Object.entries(p)) if (typeof v === "number" && k !== "t" && k !== "secs") keys.add(k);
  return [...keys].sort();
}

let openRule = null;
function onParams(r) { const id = r.params.get("rule"); if (id) openRule?.(id); }

function render(page, r) {
  const counts = h("div", { class: "row", style: { gap: "8px" } });
  const list = h("div", { class: "stack", style: { gap: "8px" } });
  const tl = h("div");
  const events = h("div", { class: "timeline" });
  const meta = h("div", { class: "small muted" });
  page.append(
    h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, "Alerts"), h("p", {}, "The server checks every rule on each point, every 2 seconds. A rule goes pending, then fires once its condition held long enough.")),
      h("div", { class: "tools" }, h("button", { class: "btn primary", onclick: () => edit(null) }, icon("plus"), "New rule"))),
    h("div", { class: "row between", style: { marginBottom: "14px" } }, counts, meta),
    card({ title: "When each rule fired", sub: "from the events of the last hour", body: tl, attrs: { style: { marginBottom: "14px" } } }),
    h("div", { class: "grid g3" },
      h("div", { class: "span2", style: { display: "contents" } }, card({ title: "Rules", sub: "drag to reorder · click to edit", body: list, cls: "span2" })),
      card({ title: "Events", sub: "newest first", body: events })));

  let rules = [];
  async function save(next, what) {
    try {
      await api("/v1/alerts/rules", { method: "PUT", body: { rules: next.map(clean) } });
      toast(what, "ok");
      await refreshAlerts();
    } catch (e) { toast(e instanceof ApiError && e.status === 403 ? "Changing rules needs an admin key" : e.message, "bad"); throw e; }
  }
  const clean = (x) => ({ id: x.id, name: x.name, metric: x.metric, op: x.op, threshold: Number(x.threshold), for_s: Number(x.for_s) || 0, severity: x.severity, enabled: !!x.enabled, description: x.description || "" });

  function draw() {
    const a = state.alerts;
    if (!a) { clear(list, empty("alerts", "Loading the rules…")); return; }
    rules = a.rules || [];
    const by = {};
    for (const x of rules) by[x.status?.state || "ok"] = (by[x.status?.state || "ok"] || 0) + 1;
    clear(counts, ["firing", "pending", "ok", "no data", "disabled"].filter((k) => by[k]).map((k) => pill(k === "no data" ? "info" : k, `${by[k]} ${k}`)));
    clear(meta, a.webhook ? [icon("ext"), " posting changes to a webhook"] : "No webhook: start with --alert-webhook URL to be told.", a.file ? ` · rules kept in ${a.file}` : " · rules kept in memory (--alerts FILE keeps them)");
    const ps = visible();
    const ordered = rules.slice();
    clear(list, ordered.map((x) => {
      const st = x.status?.state || "ok";
      const vals = ps.map((p) => p[x.metric]);
      const row = h("div", { class: "card hover", draggable: "true", dataset: { id: x.id }, style: { padding: "10px 12px", display: "grid", gridTemplateColumns: "18px 12px minmax(0,1fr) 150px 92px 40px", gap: "12px", alignItems: "center", cursor: "pointer" } },
        h("span", { class: "handle", title: "Drag to reorder" }, icon("grip")),
        h("span", { class: "dot " + (st === "firing" ? "firing pulse" : st === "pending" ? "pending" : st === "ok" ? "ok" : ""), title: st }),
        h("div", { style: { minWidth: 0 } },
          h("div", { class: "row nw", style: { gap: "8px" } }, h("b", { class: "ellipsis" }, x.name), h("span", { class: "badge" }, x.severity)),
          h("div", { class: "small muted mono ellipsis" }, `${x.metric} ${x.op} ${fmt.si(x.threshold)}${x.for_s ? ` for ${fmt.dur(x.for_s)}` : ""}`)),
        h("div", { style: { position: "relative", height: "30px" } }, sparkWithThreshold(vals, x.threshold, st === "firing" ? "var(--crit)" : SERIES[0])),
        h("div", { class: "num" }, h("div", { style: { fontWeight: 650 } }, x.status?.value == null ? "–" : fmt.si(x.status.value)), h("div", { class: "tiny muted" }, st === "ok" || st === "disabled" ? st : `${st} ${fmt.ago(x.status.since_ms)}`)),
        h("div", { onclick: (e) => e.stopPropagation() }, toggle(x.enabled, (on) => save(rules.map((y) => y.id === x.id ? { ...y, enabled: on } : y), `${x.name}: ${on ? "on" : "off"}`).catch(() => draw()), "")));
      row.onclick = (e) => { if (!e.target.closest(".switch")) edit(x); };
      return row;
    }));
    // Timeline: a row per rule that fired, red while firing.
    const [t0, t1] = [Date.now() - 3600e3, Date.now()];
    const evs = (a.events || []).slice().reverse();
    const spans = {};
    for (const e of evs) {
      const sp = spans[e.rule] ||= { name: e.name, sev: e.severity, list: [] };
      if (e.state === "firing") sp.list.push([e.ts_ms, null]);
      else if (sp.list.length && sp.list[sp.list.length - 1][1] == null) sp.list[sp.list.length - 1][1] = e.ts_ms;
    }
    const rows = Object.entries(spans);
    if (!rows.length) clear(tl, h("div", { class: "row nw", style: { gap: "10px" } }, h("span", { style: { color: "var(--good)" } }, icon("okc", "lg")), h("span", { class: "dim" }, "No rule fired in the last hour.")));
    else {
      const W = Math.max(400, tl.clientWidth || 900), rh = 24, LW = 190;
      const svgEl = s("svg", { viewBox: `0 0 ${W} ${rows.length * rh + 22}`, style: { width: "100%", height: rows.length * rh + 22 + "px" } });
      const X = (t) => LW + (t - t0) / (t1 - t0) * (W - LW - 10);
      rows.forEach(([, sp], i) => {
        svgEl.append(s("text", { x: 0, y: i * rh + 16, style: { fill: "var(--text-2)", fontSize: "12px" } }, sp.name.length > 28 ? sp.name.slice(0, 27) + "…" : sp.name),
          s("rect", { x: LW, y: i * rh + 5, width: W - LW - 10, height: rh - 10, rx: 4, style: { fill: "var(--surface-2)" } }));
        for (const [a0, a1] of sp.list) {
          const x0 = X(Math.max(a0, t0)), x1 = X(a1 ?? t1);
          const bar = s("rect", { x: x0, y: i * rh + 5, width: Math.max(3, x1 - x0), height: rh - 10, rx: 4, style: { fill: sp.sev === "critical" ? "var(--crit)" : sp.sev === "warning" ? "var(--warn)" : "var(--s1)" } });
          bar.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, sp.name), h("div", { class: "r" }, h("b", {}, a1 ? fmt.dur((a1 - a0) / 1000) : "still firing"), h("span", { class: "n" }, `${fmt.time(a0)} – ${a1 ? fmt.time(a1) : "now"}`))]));
          bar.addEventListener("pointerleave", hideTip);
          svgEl.append(bar);
        }
      });
      for (let k = 0; k <= 6; k++) { const t = t0 + k * 600e3; svgEl.append(s("text", { class: "axis", x: X(t), y: rows.length * rh + 16, "text-anchor": "middle", style: { fill: "var(--muted)", fontSize: "10.5px" } }, fmt.time(t, false))); }
      clear(tl, svgEl);
    }
    clear(events, (a.events || []).length ? a.events.slice(0, 60).map((e) => h("div", { class: "ev " + (e.state === "firing" ? "firing" : "ok") },
      h("div", { class: "row nw", style: { gap: "8px" } }, h("b", { class: "ellipsis" }, e.name), h("span", { class: "sep" }), h("span", { class: "tiny muted nowrap" }, fmt.time(e.ts_ms))),
      h("div", { class: "small dim" }, `${e.state === "firing" ? "fired" : "resolved"}: ${e.metric} ${e.value == null ? "had no value" : "was " + fmt.si(e.value)} (${e.op} ${fmt.si(e.threshold)})`)))
      : empty("clock", "No events yet", "Each time a rule fires or resolves it shows here, and goes to the webhook."));
  }

  function sparkWithThreshold(vals, thr, color) {
    const nums = vals.filter((v) => v != null && Number.isFinite(v));
    const svgEl = sparkline(vals.concat([]), { color, width: 150, height: 30, cls: "" });
    svgEl.setAttribute("style", "width:150px;height:30px");
    // Same scale as the line: sparkline scales to its own max, so draw the threshold against both.
    const own = Math.max(...nums, 0);
    if (own > 0 && thr <= own * 1.05) { const y = 30 - 3 - thr / own * 24; svgEl.append(s("line", { x1: 0, x2: 150, y1: y, y2: y, style: { stroke: "var(--crit)", strokeDasharray: "3 3", strokeWidth: 1, vectorEffect: "non-scaling-stroke" } })); }
    return svgEl;
  }

  sortable(list, (children) => {
    const ids = children.map((c) => c.dataset.id);
    if (ids.join() === rules.map((x) => x.id).join()) return;
    const next = ids.map((id) => rules.find((x) => x.id === id)).filter(Boolean);
    save(next, "Order saved").catch(() => draw());
  });

  function edit(rule) {
    const isNew = !rule;
    const x = rule ? { ...rule } : { id: "rule-" + Math.random().toString(36).slice(2, 8), name: "", metric: "p99_ms", op: ">", threshold: 100, for_s: 60, severity: "warning", enabled: true, description: "" };
    const metrics = metricsOfPoints();
    if (!metrics.includes(x.metric)) metrics.unshift(x.metric);
    const name = h("input", { class: "input", value: x.name, placeholder: "p99 above 100 ms" });
    const metric = h("select", { class: "select" }, metrics.map((m) => h("option", { value: m, selected: m === x.metric }, m)));
    const op = seg(OPS.map((o) => [o, o]), x.op, (v) => { x.op = v; sim(); });
    const thr = h("input", { class: "input", type: "number", step: "any", value: x.threshold, style: { width: "140px" } });
    const forS = h("input", { class: "input", type: "number", min: 0, value: x.for_s, style: { width: "110px" } });
    const sev = seg(SEV.map((v) => [v, v]), x.severity, (v) => { x.severity = v; });
    const desc = h("textarea", { class: "input", rows: 2, style: { fontFamily: "var(--sans)" } }); desc.value = x.description || "";
    const verdict = h("div", { class: "small" });
    const chart = timeChart({ height: 210, domain, series: [{ name: x.metric, get: (p) => p[x.metric], area: true }], threshold: { value: x.threshold, label: "threshold", onDrag: (v) => { x.threshold = +v.toPrecision(4); thr.value = x.threshold; sim(); }, onMove: (v) => { thr.value = +v.toPrecision(4); } } });
    function sim() {
      const ps = visible();
      let fires = 0, run = 0, held = 0, firing = false;
      for (let i = 0; i < ps.length; i++) {
        const ok = test(ps[i][x.metric], x.op, Number(x.threshold));
        if (ok) { held = run ? held + (ps[i].t - ps[i - 1].t) / 1000 : 0; run++; if (!firing && held >= Number(x.for_s || 0)) { firing = true; fires++; } }
        else { run = 0; held = 0; firing = false; }
      }
      const now = ps.length ? ps[ps.length - 1][x.metric] : null;
      clear(verdict, h("div", { class: "row nw", style: { gap: "8px" } }, icon(fires ? "warn" : "okc"),
        h("span", {}, `Over the ${state.zoom ? "zoomed range" : `last ${state.range} min`} this rule would have fired `, h("b", {}, fmt.plural(fires, "time")), `. Now: ${now == null ? "no value" : fmt.si(now)}.`)));
    }
    metric.onchange = () => { x.metric = metric.value; chart.remove(); const c2 = timeChart({ height: 210, domain, series: [{ name: x.metric, get: (p) => p[x.metric], area: true }], threshold: { value: x.threshold, label: "threshold", onDrag: (v) => { x.threshold = +v.toPrecision(4); thr.value = x.threshold; sim(); } } }); chartBox.replaceChildren(c2); c2.update(visible()); sim(); };
    thr.oninput = () => { x.threshold = Number(thr.value); chartBox.firstChild?.setThreshold?.(x.threshold); sim(); };
    forS.oninput = () => { x.for_s = Number(forS.value); sim(); };
    const chartBox = h("div", {}, chart);
    const saveBtn = h("button", { class: "btn primary" }, icon("check"), isNew ? "Create rule" : "Save");
    const delBtn = isNew ? null : h("button", { class: "btn danger" }, icon("trash"), "Delete");
    const d = drawer({
      title: isNew ? "New alert rule" : x.name, sub: isNew ? "fires when a point's value crosses the threshold" : `${x.id} · ${x.status?.state || ""}`, wide: true,
      body: [
        h("div", { class: "grid g2" }, h("label", { class: "f" }, "Name", name), h("label", { class: "f" }, "Metric (a field of the points)", metric)),
        h("div", { class: "row", style: { gap: "14px", alignItems: "flex-end" } },
          h("label", { class: "f" }, "Fires when the value is", op), h("label", { class: "f" }, "Threshold", thr), h("label", { class: "f" }, "for (seconds)", forS), h("label", { class: "f" }, "Severity", sev)),
        card({ title: "Drag the red line to set the threshold", sub: "the metric over the range shown", body: [chartBox, h("div", { style: { marginTop: "8px" } }, verdict)] }),
        h("label", { class: "f" }, "Description (sent with the webhook)", desc),
      ],
      foot: [delBtn, h("span", { class: "sep" }), h("button", { class: "btn", onclick: () => d.close() }, "Cancel"), saveBtn],
    });
    chart.update(visible()); sim();
    saveBtn.onclick = async () => {
      x.name = name.value.trim() || `${x.metric} ${x.op} ${x.threshold}`; x.description = desc.value.trim();
      const next = isNew ? [...rules, x] : rules.map((y) => y.id === x.id ? x : y);
      try { await save(next, isNew ? "Rule created" : "Rule saved"); d.close(); } catch { /* told */ }
    };
    if (delBtn) delBtn.onclick = async () => {
      if (!await confirm({ title: `Delete "${x.name}"?`, body: "The server stops checking it. Its past events stay.", ok: "Delete", danger: true })) return;
      try { await save(rules.filter((y) => y.id !== x.id), "Rule deleted"); d.close(); } catch { /* told */ }
    };
  }
  openRule = (id) => { const x = (state.alerts?.rules || []).find((y) => y.id === id); if (x) edit(x); };

  draw();
  const want = r.params.get("rule");
  if (want) setTimeout(() => openRule(want), 50);
  const offs = [bus.on("alerts", draw), bus.on("points", () => { if (!document.querySelector(".drawer")) draw(); })];
  return () => { offs.forEach((f) => f()); openRule = null; };
}
