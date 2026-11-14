// Timeline: what changed in the fleet, on one time axis. A summary of the
// window (overnight by default from the palette), traffic with lanes for
// alerts, copies, primaries, rollouts and config, people's changes and
// notes, and every event in a list. Each event opens the view it concerns
// at that moment; a note goes on the axis for the next person.

import { h, icon, clear, fmt, every, api, bus, seg, toast, modal, can, needs, link, when, whenTitle, span, copy, card, empty, tipOn } from "../core.js";
import { timeChart, SERIES } from "../charts.js";
import { state, visible, domain, setZoom, setRange, setWindow, timeRange, rangeParams, fresh, firingAlerts, overnight } from "../data.js";
import { eventLink, kindName, kindTone } from "./overview.js";
import { ownerOf } from "../owners.js";

export default { id: "timeline", title: "Timeline", icon: "timeline", group: "Operate", keywords: "timeline events history what changed overnight failover primary copy down rollout audit changes notes incident postmortem", render, onParams };

const LANES = [
  ["alerts", "Alerts", (k) => k.startsWith("alert_")],
  ["copies", "Copies and shards", (k) => ["copy_down", "copy_up", "copy_added", "copy_removed", "majority_lost", "majority_back", "shard_added", "shard_removed", "moves_started", "moves_finished"].includes(k)],
  ["primaries", "Primaries", (k) => k === "primary_changed"],
  ["rollouts", "Rollouts and config", (k) => ["rollout", "upgrade", "autoscale", "log_level"].includes(k)],
  ["people", "People's changes", (k) => k === "change" || k === "contain"],
  ["clients", "New client traffic", (k) => k === "new_shape"],
  ["notes", "Notes", (k) => k === "note"],
];
// Lane names on a phone.
const SHORT = { copies: "Copies", rollouts: "Rollouts", people: "People", clients: "Clients" };
const laneOf = (k) => (LANES.find(([, , f]) => f(k)) || ["other"])[0];
// Pairs drawn as a span: the event that ends each kind.
const ENDS = { copy_down: "copy_up", alert_fired: "alert_resolved", majority_lost: "majority_back", moves_started: "moves_finished" };

let reopen = null;
function onParams(r) { reopen?.(r); }

function render(page, r0) {
  let shardFilter = r0.params.get("shard"), focusId = r0.params.get("event");
  const hidden = new Set(JSON.parse(localStorage.getItem("chloedb.cc.tl.hidden") || "[]"));
  let events = [], source = "events", stats = [], statsSince = null, q = "", spans = [];
  const open = new Set(); // grouped rows shown in full

  const presets = seg([["1h", "1h"], ["6h", "6h"], ["overnight", "Overnight"], ["24h", "24h"], ["7d", "7d"]], presetNow(), (v) => {
    if (v === "overnight") { const [a, b] = overnight(); setWindow(a, b); } else setRange(v);
  }, { "aria-label": "Window" });
  const noteBtn = h("button", { class: "btn sm", onclick: () => addNote() }, icon("note"), "Add a note");
  if (!can("admin", "POST /v1/events/notes")) { noteBtn.disabled = true; noteBtn.title = "Adding a note " + needs("admin", "POST /v1/events/notes"); }
  const shareBtn = h("button", { class: "btn sm primary", title: "Copy a link to this window, fixed: it opens paused, with the same events", onclick: () => copy(location.origin + location.pathname + link("timeline", { shard: shardFilter, ...rangeParams({ absolute: true }) }), "Link to this window of the timeline copied") }, icon("share"), "Share this window");
  const head = h("div", { class: "page-head" },
    h("div", {}, h("h1", {}, "Timeline"), h("p", {}, "What changed in the fleet, on one time axis: alerts, copies and primaries, rollouts and config, people's changes and notes.")),
    h("div", { class: "tools" }, presets, noteBtn, shareBtn));
  const filterNote = h("div");
  const sumTitle = h("h3", {});
  const sumSub = h("span", { class: "sub" });
  const sumTools = h("div", { class: "tools" });
  const sumBody = h("div", { class: "tl-sum" });
  const sumCard = h("section", { class: "card tl-sumcard" }, h("div", { class: "hd" }, sumTitle, sumSub, sumTools), h("div", { class: "bd" }, sumBody));

  const chart = timeChart({ height: 110, label: "searches", series: [
    { name: "queries/s", get: (p) => p.qps, area: true, color: SERIES[0] },
  ], domain, onBrush: (a, b) => setZoom(a == null ? null : [a, b]), markers: () => [] });
  // Failures on an axis of their own, as a share of the searches: an outage
  // is a tall red block, not a dip in the traffic (NEW-scenarios-11).
  const failChart = timeChart({ height: 70, label: "failed", fmt: (v) => fmt.pct(v, v < 0.01 ? 2 : 1), series: [
    { name: "failed, share of searches", get: (p) => p.error_ratio ?? (p.qps ? (p.errors_s || 0) / p.qps : null), area: true, color: SERIES[7] },
  ], domain, onBrush: (a, b) => setZoom(a == null ? null : [a, b]), markers: () => [], threshold: { value: 0.01, label: "SLO" }, legend: false });
  const chips = h("div", { class: "row tl-chips" });
  const lanes = h("div", { class: "tl-lanes" });
  const axis = h("div", { class: "tl-axis" });
  const axisSub = h("span", { class: "sub" });
  const axisCard = h("section", { class: "card tl-axiscard" }, h("div", { class: "hd" }, h("h3", {}, "On one axis"), axisSub, h("div", { class: "tools" }, chips)),
    h("div", { class: "bd" }, h("div", { class: "tl-chartrow" }, h("div", { class: "tl-lname" }, h("b", {}, "Searches/s"), h("span", { class: "small muted" }, "this coordinator")), h("div", { class: "grow" }, chart)),
      h("div", { class: "tl-chartrow" }, h("div", { class: "tl-lname" }, h("b", {}, "Failed"), h("span", { class: "small muted" }, "share, server's side")), h("div", { class: "grow" }, failChart)), lanes, axis));

  const filt = h("input", { class: "input sm", placeholder: "Filter: shard 117, nadia, v0.9.4…", oninput: () => { q = filt.value.trim().toLowerCase(); drawList(); } });
  const listBody = h("div");
  const listSub = h("span", { class: "sub" });
  const listCard = h("section", { class: "card tl-listcard" }, h("div", { class: "hd" }, h("h3", {}, "Events"), listSub, h("div", { class: "tools" }, filt)), h("div", { class: "bd flush" }, listBody));
  page.append(head, filterNote, sumCard, axisCard, listCard);

  // ------------------------------------------------------------ reading
  async function load() {
    const r = timeRange();
    const from = Math.round(r.from), to = Math.round(r.to);
    try {
      const j = await api(`/v1/events?from=${from}&to=${to}&limit=5000`, { timeout: 15000 });
      events = (j.events || []).slice();
      source = "events";
    } catch (e) {
      if (e.status !== 404 && e.status !== 405) { if (!events.length) clear(listBody, h("div", { class: "error", style: { margin: "0 14px 14px" } }, "The events: " + e.message)); return; }
      events = await fallbackEvents(from, to);
      source = "fallback";
    }
    // The statements' statistics are an admin's: another key's timeline has the events alone.
    if (can("admin")) try {
      const j = await api(`/v1/queries/stats?from=${from}&to=${to}&limit=500`, { timeout: 10000 });
      stats = (Array.isArray(j) ? j : j.statements || []);
      statsSince = Array.isArray(j) ? null : j.since_ms ?? null;
      // New statement shapes of the window, as events on the clients' lane.
      for (const s of stats) if (s.new && s.first_seen_ms >= from && s.first_seen_ms <= to) events.push({ id: "shape-" + s.fingerprint, ts_ms: s.first_seen_ms, kind: "new_shape", severity: "info", title: `new statement shape${s.keys?.[0] ? " from key " + s.keys[0].key : ""}: ${(s.sql || s.fingerprint || "").slice(0, 90)}`, detail: { calls: s.calls, p99_ms: s.p99_ms, fingerprint: s.fingerprint }, who: s.keys?.[0]?.key });
    } catch { /* an older server, or a read key */ }
    // The operator's rollouts (F1): each spec version's span, from when it
    // was set to when every copy ran it. A version the events do not name
    // shows as a rollout; copies going down and primaries moving inside a
    // span are the rollout's, not an incident.
    spans = [];
    try {
      const k = await api("/v1/kube", { timeout: 5000 });
      for (const v of k?.status?.history || []) {
        const a = v.atMs, b = v.readyMs || (v.failed ? a : Date.now());
        if (!a || b < from || a > to) continue;
        spans.push({ from: a, to: b + 2 * 60e3, version: v.version, by: v.by, done: !!v.readyMs, failed: !!v.failed });
        if (!events.some((e) => (e.kind === "rollout" || e.kind === "upgrade") && Math.abs(e.ts_ms - a) < 10 * 60e3)) events.push({ id: `spec-v${v.version}`, ts_ms: a, kind: v.by === "upgrader" ? "upgrade" : "rollout", severity: v.failed ? "warning" : "info", title: `Spec v${v.version} rolled out${v.by ? ` by ${["operator", "upgrader", "autoscaler"].includes(v.by) ? "the " + v.by : v.by}` : ""}${v.readyMs ? `, every copy on it after ${fmt.dur((v.readyMs - a) / 1000).replace(/ 0s$/, "")}` : v.failed ? ", failed" : ", still rolling"}`, who: v.by || "operator", detail: { version: v.version } });
      }
    } catch { /* not on Kubernetes, or an older server */ }
    for (const e of events) e.during = spans.find((x) => e.ts_ms >= x.from && e.ts_ms <= x.to && DURING.has(e.kind)) || null;
    events.sort((a, b) => b.ts_ms - a.ts_ms);
    drawAll();
  }
  async function fallbackEvents(from, to) {
    const out = [];
    for (const x of state.alerts?.events || []) if (x.ts_ms >= from && x.ts_ms <= to) out.push({ id: `a-${x.rule || x.name}-${x.ts_ms}`, ts_ms: x.ts_ms, kind: x.state === "firing" ? "alert_fired" : x.state === "ok" ? "alert_resolved" : "alert_" + String(x.state).replace(/\s/g, "_"), severity: x.state === "firing" ? (x.severity || "warning") : "info", title: `${x.name || x.rule}: ${x.state}`, where: { rule: x.rule } });
    try {
      const j = await api("/v1/admin/audit?limit=1000", { timeout: 10000 });
      for (const e of j.entries || []) {
        if (e.ts_ms < from || e.ts_ms > to) continue;
        const change = e.kind ? e.kind === "change" : !!e.detail && e.method !== "GET";
        if (!change) continue;
        out.push({ id: "c-" + (e.request_id || e.ts_ms), ts_ms: e.ts_ms, kind: "change", severity: "info", title: e.detail || `${e.method} ${e.route}`, who: e.who });
      }
    } catch { /* not an admin key */ }
    return out;
  }

  // ------------------------------------------------------------ drawing
  function shown() {
    let evs = events;
    if (shardFilter != null && shardFilter !== "") evs = evs.filter((e) => String(e.where?.shard ?? "") === String(shardFilter) || (e.where?.copy && new RegExp(`^s0*${shardFilter}-`).test(e.where.copy)) || new RegExp(`\\bshard ${shardFilter}\\b`).test(e.title));
    return evs;
  }
  function drawAll() { drawFilter(); drawSummary(); drawAxis(); drawList(); }

  function drawFilter() {
    clear(filterNote, shardFilter != null && shardFilter !== "" ? h("div", { class: "notice row nw", style: { marginBottom: "14px" } }, icon("filter"), `Only shard ${shardFilter}'s events.`, h("span", { class: "sep" }), h("button", { class: "btn xs", onclick: () => { shardFilter = null; location.hash = link("timeline"); } }, "Show the whole fleet")) : null,
      source === "fallback" ? h("div", { class: "notice", style: { marginBottom: "14px" } }, "This server keeps no events (it has no /v1/events): the timeline shows the alerts' changes and the audit log's changes only. Copies going down, failovers and rollouts show once the server records them.") : null);
  }

  function drawSummary() {
    const r = timeRange();
    const evs = shown();
    const isNight = presetNow() === "overnight";
    sumTitle.textContent = isNight ? `Overnight, ${when(r.from, { sec: false })} → ${when(r.to, { sec: false }).slice(-5)}` : `In ${r.fixed ? span(r.from, r.to) : r.label.replace("last", "the last")}`;
    sumSub.textContent = `${fmt.plural(evs.length, "event")}` + (r.live ? " · live" : "");
    const probs = state.fleet?.problems || [];
    clear(sumTools, probs.length ? h("a", { class: "home-chip warn", href: link("home") }, icon("warn"), `${fmt.plural(probs.length, "thing")} need${probs.length === 1 ? "s" : ""} a look now`) : h("span", { class: "small muted" }, "nothing needs a look now"));
    const of = (f) => evs.filter((e) => f(e.kind, e));
    const incidents = incidentsOf(evs);
    const prim = of((k) => k === "primary_changed");
    const auto = prim.filter((e) => e.detail?.automatic).length;
    const rolls = of((k) => k === "rollout" || k === "upgrade");
    const changes = of((k, e) => k === "change" && e.who && !/^(operator|autoscaler|upgrader)$/.test(e.who));
    // F8: an app's changes (its own functions and tables, often under the
    // root token) are machine changes, counted apart from people's.
    const machine = changes.filter((e) => machineOf(e)), people = changes.filter((e) => !machineOf(e));
    const byApp = [...new Set(machine.map(machineOf))];
    const shapes = of((k) => k === "new_shape");
    const copiesAll = of((k) => k === "copy_down");
    const copiesDown = copiesAll.filter((e) => !e.during), copiesRolling = copiesAll.length - copiesDown.length;
    const who = [...new Set(people.map((e) => e.who))];
    const dk = (state.fleet?.checks || []).find((c) => c.id === "disk");
    const tile = (label, val, sub, tone, href) => h(href ? "a" : "div", { class: "tl-tile " + (tone || ""), href }, h("div", { class: "small dim" }, label), h("div", { class: "v" }, val), h("div", { class: "small muted", title: typeof sub === "string" ? sub : null }, sub));
    // The server cannot tell "new" for a window that starts before it kept statements.
    const blind = statsSince != null && statsSince + 60e3 > r.from;
    clear(sumBody,
      tile("Incidents", String(incidents.length), incidents.length ? incidents.slice(0, 2).map((x) => `${x.what} (${when(x.from, { sec: false })}, ${x.open ? "still open" : fmt.dur((x.to - x.from) / 1000).replace(/ 0s$/, "")}${x.n > 1 ? `, ${x.n} signals` : ""})`).join(" · ") : "no shard lost its majority, no critical alert", incidents.length ? "crit" : "", incidents[0] ? eventLink(incidents[0].first) : null),
      tile("Copies down", String(copiesDown.length), [copiesDown.length ? [...new Set(copiesDown.map((e) => e.where?.copy || e.title))].slice(0, 3).join(", ") : copiesRolling ? "none outside a rollout" : "every copy stayed up", copiesRolling ? `${copiesRolling} more restarted by a rollout, as planned` : null].filter(Boolean).join(" · "), copiesDown.length ? "warn" : ""),
      tile("Primaries moved", String(prim.length), prim.length ? [auto ? `${auto} failover${auto === 1 ? "" : "s"}` : null, prim.length - auto ? `${prim.length - auto} handed over` : null].filter(Boolean).join(" · ") : "no primary moved", auto ? "warn" : ""),
      tile("Rollouts", rolls.length ? String(rolls.length) : "0", rolls.length ? rolls[0].title : "no rollout", "", rolls[0] ? (rolls[0].id?.startsWith("spec-") ? link("rollout", { tab: "history" }) : eventLink(rolls[0])) : null),
      tile("Changes by people", String(people.length), [people.length ? who.slice(0, 3).join(", ") + (who.length > 3 ? ` +${who.length - 3}` : "") : "nobody changed anything", machine.length ? `${machine.length} more by ${byApp.join(", ")}` : null].filter(Boolean).join(" · "), ""),
      tile("New client traffic", blind && !shapes.length ? "?" : String(shapes.length), shapes.length ? shapes.slice(0, 2).map((e) => e.who ? "key " + e.who : "a new shape").join(" · ") + (blind ? ` · others can't be told: this server started at ${when(statsSince, { sec: false })}` : "") : blind ? `Can't tell: this server started at ${when(statsSince, { sec: false })}, after the window began` : stats.length && !stats.some((s) => "new" in s) ? "this server does not mark new shapes" : "no new statement shape", shapes.length ? "warn" : ""),
      tile("Capacity", dk?.max != null ? fmt.pct(dk.max, 0) : "–", dk?.max != null ? `fullest disk now, ${dk.where || ""}${dk.days_to_full != null ? ` · full in about ${fmt.fills(dk.days_to_full)}` : ""}` : "not known", dk?.max > 0.85 ? "warn" : "", link("cluster", { tab: "capacity" })),
    );
  }
  function drawAxis() {
    chart.update(visible());
    failChart.update(visible());
    axisCard.classList.toggle("stale", !fresh("series").ok && !fresh("series").loading);
    const [t0, t1] = domain();
    const span_ = Math.max(1, t1 - t0);
    const x = (t) => `calc(46px + (100% - 60px) * ${Math.min(1, Math.max(0, (t - t0) / span_)).toFixed(5)})`;
    const evs = shown().filter((e) => (e.until_ms || e.ts_ms) >= t0 && e.ts_ms <= t1);
    const vs = visible();
    const from = vs.length && vs[0].t - Math.min(t0, timeRange().from) > 5 * 60e3 ? ` · data from ${when(vs[0].t, { sec: false })}, when this server started keeping it` : "";
    axisSub.textContent = `drag to zoom · click an event to open its view at that moment · ${state.series.source === "load" ? "a point a minute" : "a point every " + fmt.dur(state.interval / 1000)}${from}`;
    clear(chips, LANES.map(([id, name]) => {
      const n = evs.filter((e) => laneOf(e.kind) === id).length;
      const b = h("button", { class: "chip", "aria-pressed": String(!hidden.has(id)), title: hidden.has(id) ? "Show this lane" : "Hide this lane" }, name.split(" ")[0].toLowerCase(), n ? h("span", { class: "muted" }, " " + n) : null);
      b.onclick = () => { if (hidden.has(id)) hidden.delete(id); else hidden.add(id); try { localStorage.setItem("chloedb.cc.tl.hidden", JSON.stringify([...hidden])); } catch { /* private mode */ } drawAxis(); };
      return b;
    }));
    clear(lanes, LANES.filter(([id]) => !hidden.has(id)).map(([id, name]) => {
      const mine = evs.filter((e) => laneOf(e.kind) === id).sort((a, b) => a.ts_ms - b.ts_ms);
      const track = h("div", { class: "tl-track" });
      const used = new Set(), placed = [];
      // Spans: a start and its end (a copy down and back, an alert fired and resolved).
      for (const e of mine) {
        if (used.has(e)) continue;
        const endKind = ENDS[e.kind];
        let end = null;
        if (endKind) { end = mine.find((y) => !used.has(y) && y.ts_ms >= e.ts_ms && y.kind === endKind && sameWhere(y, e)); if (end) used.add(end); }
        used.add(e);
        const tone = kindTone(e) || "info";
        const until = end ? end.ts_ms : e.until_ms || (endKind ? Math.min(t1, Date.now()) : null);
        const el = h("a", { class: "tl-ev " + tone + (until ? " span" : "") + (e.id === focusId ? " focus" : ""), href: eventLink(e), style: { left: x(e.ts_ms), ...(until ? { width: `calc((100% - 60px) * ${Math.max(0.002, (Math.min(until, t1) - Math.max(e.ts_ms, t0)) / span_).toFixed(5)})` } : {}) } },
          h("i"), h("span", { class: "lbl" }, laneLabel(e, end)));
        tipOn(el, () => [h("div", { class: "t" }, whenTitle(e.ts_ms)), h("div", {}, h("b", {}, e.title)), end ? h("div", { class: "small" }, `${end.title} at ${when(end.ts_ms)} (${fmt.dur(spanSecs(e, end))})`) : endKind ? h("div", { class: "small" }, "not over at the end of the window") : null, e.who ? h("div", { class: "small muted" }, "by " + e.who) : null, h("div", { class: "small muted" }, "click: open its view at that moment")]);
        track.append(el);
        placed.push({ el, f: (Math.max(e.ts_ms, t0) - t0) / span_, fe: until ? (Math.min(until, t1) - t0) / span_ : null, n: laneLabel(e, end).length });
      }
      // Labels that would overlap give way (hover still shows them); one
      // near the right edge goes to the left of its dot instead.
      const nameW = parseFloat(getComputedStyle(axisCard).getPropertyValue("--tl-name")) || 150;
      const W = Math.max(120, (lanes.clientWidth || 1000) - nameW - 60);
      // Spans that overlap in time go on rows of their own (a copy down
      // under the shard that lost its majority), at most three.
      // Dots at one moment share a row: only a span keeps a row busy.
      const rowEnd = [-1];
      for (const p of placed) {
        let r = rowEnd.findIndex((e) => e < p.f - 8 / W);
        if (r < 0) r = rowEnd.length < 3 ? rowEnd.length : rowEnd.indexOf(Math.min(...rowEnd));
        if (p.fe != null) rowEnd[r] = Math.max(rowEnd[r] ?? -1, p.fe);
        else if (rowEnd[r] == null) rowEnd[r] = -1;
        p.row = r;
        p.el.style.top = (3 + r * 16) + "px";
      }
      const rows = Math.max(1, rowEnd.length);
      track.style.height = (6 + rows * 16) + "px";
      const taken = [];
      for (const p of placed) {
        const lw = (Math.min(p.n, 44) * 6.4 + 14) / W;
        let a = p.fe != null ? p.fe + 6 / W : p.f + 10 / W, b = a + lw;
        if (b > 1.02) { a = p.f - lw - 4 / W; b = p.f; p.el.classList.add("left"); }
        const same = placed.filter((o) => o !== p && o.row === p.row);
        const blocked = taken.some(([x, y, r]) => r === p.row && a < y && b > x) || same.some((o) => (o.fe != null ? o.f < b && o.fe > a : o.f > a - 4 / W && o.f < b + 4 / W));
        if (a < -0.02 || blocked) p.el.classList.add("nolbl");
        else taken.push([a, b, p.row]);
      }
      return h("div", { class: "tl-lane" + (rows > 1 ? " tall" : "") }, h("div", { class: "tl-lname", title: name }, h("span", { class: "tl-full" }, name), h("span", { class: "tl-short" }, SHORT[id] || name), mine.length ? h("span", { class: "muted" }, " " + mine.length) : null), track);
    }));
    // The time axis under the lanes.
    const ticks = [];
    const steps = [60e3, 300e3, 900e3, 1800e3, 3600e3, 7200e3, 3 * 3600e3, 6 * 3600e3, 12 * 3600e3, 86400e3];
    // As many ticks as fit: about one per 64 px.
    const axisW = (lanes.clientWidth || 1000) - (parseFloat(getComputedStyle(axisCard).getPropertyValue("--tl-name")) || 150) - 60;
    const want = Math.max(2, Math.min(8, Math.floor(axisW / 64)));
    const step = steps.find((s) => span_ / s <= want) || 86400e3;
    const off = new Date().getTimezoneOffset() * 60e3;
    for (let t = Math.ceil((t0 - off) / step) * step + off; t <= t1; t += step) ticks.push(t);
    clear(axis, ticks.map((t) => h("span", { style: { left: x(t) } }, step >= 86400e3 ? new Date(t).toLocaleDateString("en-US", { month: "short", day: "numeric" }) : when(t, { sec: false }).slice(-5))));
  }
  const laneLabel = (e, end) => {
    const t = e.title.length > 46 ? e.title.slice(0, 44) + "…" : e.title;
    return end ? `${t} · ${fmt.dur(spanSecs(e, end))}` : t;
  };
  // How long it lasted: a copy that comes back says how long it was down,
  // which is longer than the two events' distance when the coordinator saw it late.
  // A shard's majority came back with its copies: they say how long.
  const spanSecs = (e, end) => {
    let s = Math.max((end.ts_ms - e.ts_ms) / 1000, +end.detail?.down_s || 0);
    if (end.kind === "majority_back") for (const x of events) if (x.kind === "copy_up" && x.ts_ms === end.ts_ms && x.where?.shard === end.where?.shard) s = Math.max(s, +x.detail?.down_s || 0);
    return s;
  };

  function drawList() {
    let evs = shown();
    if (q) evs = evs.filter((e) => `${e.title} ${e.who || ""} ${kindName(e.kind)} ${JSON.stringify(e.where || {})} ${JSON.stringify(e.detail || {})}`.toLowerCase().includes(q));
    // Repeats show once with a count: the same kind and the same words but
    // for names and numbers ("primary of shard 3 handed over"), within 30
    // minutes of each other (NEW-shell-monitor-16).
    const rows = [], byKey = new Map();
    for (const e of evs) {
      const app = e.kind === "change" ? machineOf(e) : null;
      const k = app ? `machine|${app}` : `${e.kind}|${e.who || ""}|${shapeOf(e.title)}`;
      const p = byKey.get(k);
      if (p && (app || p.first - e.ts_ms < 30 * 60e3)) { p.all.push(e); p.first = e.ts_ms; continue; }
      const row = { e, all: [e], first: e.ts_ms, k };
      byKey.set(k, row); rows.push(row);
    }
    const lines = rows.flatMap((r) => open.has(r.k + r.e.ts_ms) ? [r, ...r.all.slice(1).map((e) => ({ e, all: [e], first: e.ts_ms, sub: true }))] : [r]);
    listSub.textContent = `newest first · ${fmt.plural(evs.length, "event")} in the window${rows.length < evs.length ? " · repeats grouped" : ""}`;
    if (!rows.length) { clear(listBody, empty("timeline", q ? "No event matches" : "Nothing happened in this window", source === "events" ? "Copies going down or up, primaries moving, alerts, rollouts, people's changes and notes show here as they happen." : "This server keeps no events.")); return; }
    clear(listBody, h("div", { class: "tbl-wrap", style: { maxHeight: "520px" } }, h("table", { class: "t tl-list" },
      h("thead", {}, h("tr", {}, h("th", {}, "Time"), h("th", {}, "Kind"), h("th", {}, "What"), h("th", {}, "Who"), h("th", {}, ""))),
      h("tbody", {}, lines.slice(0, 400).map((row) => { const { e, all, first, sub } = row; const n = all.length; return h("tr", { class: (e.id === focusId ? "sel" : "") + (sub ? " tl-sub" : "") },
        h("td", { class: "mono small nowrap tl-t", title: whenTitle(e.ts_ms) }, when(e.ts_ms)),
        h("td", { class: "tl-k" }, h("span", { class: "home-kind " + kindTone(e) }, kindName(e.kind))),
        h("td", { class: "tl-w" }, h("div", { class: e.severity === "critical" ? "tl-strong" : "" }, row.k?.startsWith("machine|") && !sub ? `${fmt.plural(n, "change")} by ${machineOf(e)} (its own functions and tables)` : n > 1 ? groupTitle(all) : e.title,
          e.during ? h("span", { class: "badge", title: `Inside the rollout of spec v${e.during.version}: expected while copies restart` }, `rollout v${e.during.version}`) : null,
          n > 1 ? h("button", { class: "btn xs ghost tl-more", "aria-expanded": String(open.has(row.k + e.ts_ms)), title: `${n} like it, from ${when(first)} to ${when(e.ts_ms)}`, onclick: () => { const id = row.k + e.ts_ms; if (open.has(id)) open.delete(id); else open.add(id); drawList(); } }, open.has(row.k + e.ts_ms) ? "hide them" : `×${n}, from ${when(first, { sec: false })}`) : null),
          detailText(e) ? h("div", { class: "small muted" }, detailText(e)) : null,
          rawOf(e)),
        h("td", { class: "small tl-who" }, e.who || h("span", { class: "muted" }, "–")),
        h("td", { class: "nowrap tl-go" }, h("a", { class: "btn xs", href: eventLink(e) }, `open at ${when(e.ts_ms, { sec: false }).slice(-5)}`))); })))));
  }
  const detailText = (e) => {
    const d = e.detail; if (!d || typeof d !== "object") return typeof d === "string" ? d : "";
    if (e.kind === "primary_changed") return [d.from && `from ${d.from}`, d.to && `to ${d.to}`, d.term != null && `term ${d.term}`, d.automatic != null && (d.automatic ? "failover" : "handed over")].filter(Boolean).join(" · ");
    if (e.kind === "new_shape") return [d.calls != null && fmt.plural(d.calls, "call"), d.p99_ms != null && "p99 " + fmt.ms(d.p99_ms)].filter(Boolean).join(" · ");
    // People's words for what a person reads; the raw fields are behind "details" (NEW-scenarios-10).
    const s = Object.entries(d).map(([k, v]) => human(k, v, e)).filter(Boolean).join(" · ");
    return s.length > 160 ? s.slice(0, 158) + "…" : s;
  };

  // ------------------------------------------------------------ a note
  function addNote(at) {
    const loc = (ms) => new Date(ms - new Date(ms).getTimezoneOffset() * 60e3).toISOString().slice(0, 16);
    const text = h("textarea", { class: "input", rows: 3, placeholder: "What you saw or did: \"node ip-10-4-7-21 kubelet down, asked infra\"", style: { fontFamily: "var(--sans)", fontSize: "14px" } });
    const t = h("input", { class: "input", type: "datetime-local", value: loc(at || Date.now()) });
    const shard = h("input", { class: "input", placeholder: "optional: 117", value: shardFilter || "", style: { width: "110px" } });
    const ok = h("button", { class: "btn primary" }, "Add the note");
    const m = modal({ title: "Add a note to the timeline", body: [h("p", { class: "dim", style: { margin: 0 } }, "Everyone who opens the timeline sees it at that time, with your key's name. It is kept with the events and in the audit log."), text, h("div", { class: "row nw" }, h("label", { class: "f grow" }, "At", t), h("label", { class: "f" }, "Shard", shard))], foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), ok] });
    ok.onclick = async () => {
      const body = { text: text.value.trim(), ts_ms: new Date(t.value).getTime() || Date.now() };
      if (!body.text) { text.focus(); return; }
      if (shard.value.trim() !== "") body.where = { shard: Number(shard.value) };
      ok.disabled = true;
      try { await api("/v1/events/notes", { body }); m.close(); toast("Note added", "ok"); load(); }
      catch (e) { ok.disabled = false; toast(e.status === 404 ? "This server keeps no notes: it has no /v1/events (an older server)" : "The note was not added: " + e.message, "bad"); }
    };
    setTimeout(() => text.focus(), 0);
  }

  reopen = (r) => {
    const s = r.params.get("shard"); focusId = r.params.get("event");
    if (s !== shardFilter) { shardFilter = s; drawAll(); }
    if (r.params.get("note")) addNote();
    presets.set(presetNow());
  };
  if (r0.params.get("note")) setTimeout(() => addNote(), 50);
  const offs = [
    bus.on("points", drawAxis), bus.on("range", () => { presets.set(presetNow()); load(); drawAxis(); }), bus.on("theme", drawAxis), bus.on("narrow", drawAxis), bus.on("fleet", drawSummary),
    every(20000, load),
  ];
  return () => { offs.forEach((f) => f()); reopen = null; };
}

// ---------------------------------------------------------------- helpers
/** Incidents: majority lost, critical alerts and copies down within 15 min of each other, as one. */
export function incidentsOf(evs) {
  // A copy a rollout restarts is not a signal of an incident (F1).
  const sig = evs.filter((e) => e.kind === "majority_lost" || (e.kind === "alert_fired" && e.severity === "critical") || (e.kind === "copy_down" && !e.during)).sort((a, b) => a.ts_ms - b.ts_ms);
  const endOf = (e) => { const end = evs.find((x) => x.ts_ms >= e.ts_ms && x.kind === ENDS[e.kind] && sameWhere(x, e)); return end ? end.ts_ms : null; };
  const out = [];
  for (const e of sig) {
    const end = endOf(e);
    const last = out[out.length - 1];
    if (last && e.ts_ms <= (last.open ? Infinity : last.to) + 15 * 60e3) {
      last.n++; last.all.push(e);
      if (end == null) last.open = true; else last.to = Math.max(last.to, end);
      if (rank(e) < rank(last.first)) { last.what = whatOf(e); }
      continue;
    }
    out.push({ first: e, all: [e], n: 1, from: e.ts_ms, to: end ?? e.ts_ms, open: end == null, what: whatOf(e) });
  }
  // Copies down alone, with nothing critical, are not an incident.
  return out.filter((x) => x.all.some((e) => e.kind !== "copy_down")).reverse();
}
/** Kinds a rollout makes happen: marked as its own when inside its span. */
const DURING = new Set(["copy_down", "copy_up", "primary_changed", "majority_lost", "majority_back", "alert_fired", "alert_resolved"]);
/** The app a change was made by, from the objects it names (sa_*, ctx_*…), or null for a person's. */
export function machineOf(e) {
  if (e.kind !== "change") return null;
  if (/^app:/.test(e.who || "")) return e.who.slice(4);
  const text = `${e.title || ""} ${typeof e.detail === "string" ? e.detail : JSON.stringify(e.detail || {})}`;
  const m = text.match(/\b(?:FUNCTION|TABLE|INTO|FROM|UPDATE|function|table)\s+(?:IF\s+(?:NOT\s+)?EXISTS\s+)?"?([A-Za-z_][\w]*)/) || text.match(/\b((?:sa|ctx|sheets|crawl|swarm|cc)_[\w]+)/);
  const o = m ? ownerOf(m[1]) : null;
  return o?.machinery ? o.owner : null;
}
const rank = (e) => e.kind === "majority_lost" ? 0 : e.kind === "alert_fired" ? 1 : 2;
const whatOf = (e) => { const t = e.title || kindName(e.kind); return t.length > 44 ? t.slice(0, 42) + "…" : t; };
const sameWhere = (a, b) => JSON.stringify(a.where || {}) === JSON.stringify(b.where || {});
/** A title without its names and numbers: what repeats. */
const shapeOf = (t) => String(t || "").replace(/\bs\d+-\d+\b/g, "#").replace(/\b[\w.-]+:\d+\b/g, "#").replace(/\d+([.,]\d+)?/g, "#").replace(/"[^"]*"/g, "#");
/** Repeats as one line: "13 like: Primary of shard 3 handed over (shard 0, shard 1, … +9)". */
function groupTitle(all) {
  const names = [...new Set(all.map((e) => e.where?.copy || (e.where?.shard != null ? "shard " + e.where.shard : null)).filter(Boolean))];
  return `${all.length} like: ${all[0].title}${names.length > 1 ? ` (${names.slice(0, 4).join(", ")}${names.length > 4 ? ` +${names.length - 4}` : ""})` : ""}`;
}
// Fields a person does not need in the list (they stay in the details).
const RAW = new Set(["op", "eval", "metric", "rule", "id", "ts_ms", "request_id", "fingerprint", "window_s", "for_s", "resolve", "scope", "builtin", "change", "kind", "route", "method", "status"]);
function human(k, v, e) {
  if (v == null || typeof v === "object" || RAW.has(k)) return null;
  if (String(e.title || "").includes(String(v))) return null;
  const name = k.replace(/_ms$|_s$/, "").replace(/_/g, " ");
  if (/_ms$/.test(k) && typeof v === "number") return v > 1e12 ? `${name} ${when(v)}` : `${name} ${fmt.ms(v)}`;
  if (/_s$/.test(k) && typeof v === "number") return `${name} ${fmt.dur(v)}`;
  if (typeof v === "number") return `${name} ${Number.isInteger(v) ? fmt.n(v) : fmt.n(v, Math.abs(v) < 1 ? 3 : 1)}`;
  if (typeof v === "boolean") return v ? name : null;
  return `${name} ${String(v).length > 60 ? String(v).slice(0, 58) + "…" : v}`;
}
/** Every field of an event, raw, behind "details". */
function rawOf(e) {
  if (!e.detail && !e.where) return null;
  const pre = h("pre", { class: "tl-raw mono small" });
  const d = h("details", { class: "tl-details" }, h("summary", { class: "small muted" }, "details"), pre);
  d.addEventListener("toggle", () => { if (d.open && !pre.textContent) pre.textContent = JSON.stringify({ where: e.where, detail: e.detail, who: e.who }, null, 1); });
  return d;
}
function presetNow() {
  const r = timeRange();
  // Overnight starts at yesterday's 18:00; before 08:00 it ends now, whenever it was picked.
  if (r.fixed) { const [a] = overnight(); return r.from === a ? "overnight" : "none"; }
  return r.range;
}
