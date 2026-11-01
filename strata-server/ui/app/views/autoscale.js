// Autoscaling: the cluster's rules (spec.autoscale), written as sentences
// with a word to click for each setting, and the past week replayed on
// them at once: how busy the copies would have been against the cluster
// at a fixed size, how many there would have been, each change by day,
// and what the freeze and the cooldown held back. The operator runs the
// rules on the live numbers: it acts, or asks a person (the suggestion
// waits here, to apply or dismiss), or is off. The replay reads the load
// every server keeps a row a minute of (its log store's `load` table).

import { h, s, icon, clear, fmt, card, api, toast, confirm, seg, pill, empty, tipOn, showTip, hideTip, debounce, failure } from "../core.js";
import { state, isCoordinator } from "../data.js";

export default {
  id: "autoscale", title: "Autoscaling", icon: "autoscale", group: "Admin",
  keywords: "autoscale autoscaling autoscaler scale up down out in grow shrink copies replicas busy cpu cores volumes disk full freeze cooldown rules replay week suggest approve kubernetes operator",
  render,
};

const MIN = 60e3;
const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const same = (a, b) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
const pct = (v) => (v == null ? "–" : Math.round(v * 100) + "%");
const mins = (m) => (m >= 60 && m % 60 === 0 ? `${m / 60} h` : `${m} min`);
const plural = (n, w, ws) => fmt.plural(n, w, ws);
const copies = (n) => plural(n, "copy", "copies");
/** A time as the rules read it, in UTC: Tue 14:20. */
function when(ms) {
  const d = new Date(ms);
  return `${DAYS[(d.getUTCDay() + 6) % 7]} ${String(d.getUTCHours()).padStart(2, "0")}:${String(d.getUTCMinutes()).padStart(2, "0")}`;
}
const dayOf = (ms) => Math.floor(ms / 86400e3);
function dayLabel(day) {
  const d = new Date(day * 86400e3);
  return `${DAYS[(d.getUTCDay() + 6) % 7]} ${d.getUTCDate()}`;
}
const hours = (h0) => (h0 < 1 ? `${Math.round(h0 * 60)} min` : `${h0 >= 10 ? Math.round(h0) : h0.toFixed(1)} h`);

/** What a rule starts as when it is switched on (the operator's defaults). */
const DEFAULTS = {
  grow: { busyAbove: 0.7, forMinutes: 10, add: 1, max: 5 },
  shrink: { busyBelow: 0.35, forMinutes: 60, min: 2 },
  volumes: { fullAbove: 0.8, action: "grow" },
  freeze: { from: "Fri 16:00", to: "Mon 08:00", allowGrow: true },
};

/**
 * The rules, as sentences: text and words, each word a setting with its
 * choices. `kind` is the pill; a rule without `off` is always on.
 */
const RULES = [
  {
    id: "grow", kind: "grow", label: "Add copies",
    parts: (r) => ["When the busiest shard's copies use more than ", w("busyAbove", r, [0.5, 0.6, 0.7, 0.8, 0.9], pct), " of their cores for ", w("forMinutes", r, [5, 10, 20, 30, 60], mins),
      ", add ", w("add", r, [1, 2], (v) => `${copies(v)} to every shard`), ", up to ", w("max", r, [3, 4, 5, 6, 8, 10], copies), "."],
  },
  {
    id: "shrink", kind: "shrink", label: "Remove copies",
    parts: (r) => ["When they use less than ", w("busyBelow", r, [0.2, 0.25, 0.35, 0.45, 0.5], pct), " for ", w("forMinutes", r, [30, 60, 180, 360], mins),
      ", remove a copy from every shard, down to ", w("min", r, [1, 2, 3], copies), "."],
  },
  {
    id: "volumes", kind: "data", label: "Volumes",
    parts: (r) => ["When the fullest volume is more than ", w("fullAbove", r, [0.7, 0.8, 0.9], (v) => `${pct(v)} full`), ", ",
      w("action", r, ["grow", "addShard"], (v) => (v === "grow" ? "grow every volume by half" : "add a shard (the slots even out)")), "."],
  },
  {
    id: "freeze", kind: "guard", label: "Freeze",
    parts: (r) => ["Make no change from ", w("from", r, ["Fri 12:00", "Fri 16:00", "Fri 20:00", "Sat 00:00"], String), " to ", w("to", r, ["Mon 06:00", "Mon 08:00", "Mon 10:00"], String), " (UTC), ",
      w("allowGrow", r, [true, false], (v) => (v ? "except to add copies" : "not even to add copies")), "."],
  },
  {
    id: "cooldown", kind: "guard", label: "Cooldown", always: true,
    parts: (r) => ["Wait at least ", w("cooldownMinutes", r, [10, 20, 30, 60], mins), " after a change before the next, and count a new copy only once it has caught up (about 20 min)."],
  },
];
function w(key, value, choices, show) { return { key, value: value?.[key], choices, show }; }

function render(page) {
  const head = h("div"), live = h("div"), main = h("div", { class: "as-main" });
  const left = h("div", { class: "as-col" }), right = h("div", { class: "as-col" });
  main.append(left, right);
  page.append(head, live, main);

  let kube = null;            // GET /v1/kube
  let draft = null;           // the rules being edited, as spec.autoscale (a rule switched off is null)
  let kept = {};              // a switched-off rule's settings, for when it is on again
  let from = null;            // the rules the draft was taken from
  let editing = null;         // "rule.key" whose choices are open
  let replay = null, replayErr = null, replaying = false;
  let fixed = null;           // copies of each shard the fixed-size cluster has
  let day = null;             // the day whose events show
  let busy = false;
  let alive = true;
  const managed = () => !!kube?.managed;
  const spec = () => kube?.spec || {};
  const status = () => kube?.status?.autoscale || null;

  /** spec.autoscale with the operator's defaults, as the draft holds it. */
  function rulesOf(a) {
    a = a || {};
    return {
      mode: a.mode || "off",
      grow: a.grow ? { ...DEFAULTS.grow, ...a.grow } : null,
      shrink: a.shrink ? { ...DEFAULTS.shrink, ...a.shrink } : null,
      volumes: a.volumes ? { ...DEFAULTS.volumes, ...a.volumes } : null,
      freeze: (a.freeze || []).map((f) => ({ ...DEFAULTS.freeze, ...f })),
      cooldownMinutes: a.cooldownMinutes ?? 20,
    };
  }
  /** A cluster with no rules yet starts from the operator's defaults, switched on but not running (mode off). */
  function starting(a) {
    const r = rulesOf(a);
    if (!a || (!a.grow && !a.shrink && !a.volumes && !(a.freeze || []).length)) {
      Object.assign(r, { grow: { ...DEFAULTS.grow }, shrink: { ...DEFAULTS.shrink }, volumes: { ...DEFAULTS.volumes }, freeze: [{ ...DEFAULTS.freeze }] });
    }
    return r;
  }
  const dirty = () => !!draft && !same(draft, rulesOf(spec().autoscale));
  const isOn = (id) => (id === "freeze" ? draft.freeze.length > 0 : id === "cooldown" ? true : !!draft[id]);
  const valueOf = (id) => (id === "freeze" ? draft.freeze[0] || kept.freeze || DEFAULTS.freeze : id === "cooldown" ? draft : draft[id] || kept[id] || DEFAULTS[id]);

  // ---------------------------------------------------------- loading
  async function load() {
    try { kube = await api("/v1/kube"); } catch (e) { kube = { managed: false, error: e }; }
    const now = spec().autoscale;
    if (!draft || (!dirty() && !same(from, now))) {
      draft = starting(now);
      from = now ?? null;
      fixed = null;
      replayNow();
    }
    draw();
  }
  async function replayNow() {
    replaying = true;
    drawReplay();
    try {
      replay = await api("/v1/admin/autoscale/replay", { body: { autoscale: draft, days: 7 } });
      replayErr = null;
      if (fixed == null) fixed = replay.sizes?.replicas || 3;
    } catch (e) { replayErr = e; }
    replaying = false;
    if (alive) { drawRules(); drawReplay(); }
  }
  const replaySoon = debounce(replayNow, 250);

  // ---------------------------------------------------------- editing
  function set(id, key, v) {
    if (id === "freeze") draft.freeze = [{ ...valueOf("freeze"), [key]: v }];
    else if (id === "cooldown") draft.cooldownMinutes = v;
    else draft[id] = { ...valueOf(id), [key]: v };
    // Rules that would undo each other: the other moves out of the way.
    if (draft.grow && draft.shrink && draft.shrink.busyBelow >= draft.grow.busyAbove) {
      if (id === "grow") draft.shrink = { ...draft.shrink, busyBelow: Math.max(0.1, Math.round((draft.grow.busyAbove - 0.2) * 100) / 100) };
      else draft.grow = { ...draft.grow, busyAbove: Math.min(0.95, Math.round((draft.shrink.busyBelow + 0.2) * 100) / 100) };
    }
    if (draft.grow && draft.shrink && draft.shrink.min > draft.grow.max) {
      if (id === "grow") draft.shrink = { ...draft.shrink, min: draft.grow.max }; else draft.grow = { ...draft.grow, max: draft.shrink.min };
    }
    editing = null;
    changed();
  }
  function switchRule(id, on) {
    if (id === "freeze") { if (on) draft.freeze = [{ ...(kept.freeze || DEFAULTS.freeze) }]; else { kept.freeze = draft.freeze[0]; draft.freeze = []; } }
    else if (on) draft[id] = { ...(kept[id] || DEFAULTS[id]) };
    else { kept[id] = draft[id]; draft[id] = null; }
    changed();
  }
  function setMode(m) { draft.mode = m; changed(); }
  function changed() { drawHead(); drawRules(); replaySoon(); }
  function reset() { draft = rulesOf(spec().autoscale); if (!spec().autoscale) draft = starting(null); editing = null; changed(); }

  /** The patch of spec.autoscale that makes the draft. */
  function patch() { return { autoscale: { ...draft, freeze: draft.freeze } }; }
  async function save() {
    let plan;
    try { plan = await api("/v1/admin/kube/plan", { body: { spec: patch() } }); }
    catch (e) { toast("Not saved: " + e.message, "bad", 8000); return; }
    if (plan.refused) { toast("Not saved: " + plan.refused, "bad", 8000); return; }
    const m = { off: "The rules are kept and do nothing: the replay shows what they would do.", suggest: "A change the rules make waits here for a person to apply it.", on: "The operator makes the changes the rules make, and notes each one as the autoscaler's." }[draft.mode];
    const ok = await confirm({
      title: `Save the rules of ${kube.namespace}/${kube.name}`,
      body: h("div", { class: "stack", style: { gap: "8px" } },
        h("div", {}, m),
        h("ol", { class: "ctl-steps" }, (plan.steps || []).map((x) => h("li", {}, x))),
        replay?.summary ? h("div", { class: "small dim" }, `On the week replayed they make ${plural(replay.summary.changes, "change")}.`) : null),
      ok: draft.mode === "on" ? "Save, and let it act" : "Save",
    });
    if (!ok) return;
    busy = true; drawHead();
    try {
      kube = await api("/v1/admin/kube", { body: { spec: patch() } });
      from = kube.spec?.autoscale ?? null;
      draft = rulesOf(from);
      toast(draft.mode === "on" ? "The operator runs the new rules" : draft.mode === "suggest" ? "The rules ask a person from now on" : "The rules are saved, and off", "ok");
    } catch (e) { toast("Not saved: " + e.message, "bad", 8000); }
    busy = false; draw();
  }
  async function apply(sg) {
    const ok = await confirm({ title: "Apply the autoscaler's change", body: h("div", { class: "stack", style: { gap: "8px" } }, h("div", {}, sg.text), h("div", { class: "small dim" }, "The spec changes as below; the operator then makes it, one step at a time."), h("pre", { class: "code" }, JSON.stringify(sg.patch, null, 2))), ok: "Apply it" });
    if (!ok) return;
    busy = true; drawLive();
    try { kube = await api("/v1/admin/kube", { body: { spec: sg.patch } }); toast("Applied: the operator makes the change", "ok"); }
    catch (e) { toast("Not applied: " + e.message, "bad", 8000); }
    busy = false; draw();
  }
  async function dismiss() {
    busy = true; drawLive();
    try { kube = await api("/v1/admin/kube/autoscale/dismiss", { method: "POST", body: {} }); toast("Dismissed: the rules wait their cooldown before they ask again", "ok"); }
    catch (e) { toast("Not dismissed: " + e.message, "bad", 8000); }
    busy = false; draw();
  }

  // ---------------------------------------------------------- drawing
  function draw() {
    if (!kube) { clear(head, h("div", { class: "dim small" }, "Loading the cluster…")); return; }
    drawHead(); drawLive(); drawRules(); drawReplay();
  }

  function drawHead() {
    if (!draft) return;
    const tools = [
      h("span", { class: "small dim" }, "When a rule fires"),
      seg([["on", "Act", "The operator makes the change"], ["suggest", "Ask a person", "The change waits here for someone to apply it"], ["off", "Off", "Nothing changes; the week still replays"]], draft.mode, setMode, { "aria-label": "When a rule fires" }),
    ];
    if (dirty()) {
      tools.push(h("button", { class: "btn sm", disabled: busy, onclick: reset }, icon("undo"), "Undo edits"));
      tools.push(h("button", { class: "btn sm primary", disabled: busy || !managed(), title: managed() ? "Save the rules to the StrataCluster's spec" : "Only a cluster the Kubernetes operator runs keeps rules", onclick: save }, icon("check"), "Save rules"));
    }
    clear(head, h("div", { class: "page-head" },
      h("div", {},
        h("h1", {}, "Autoscaling"),
        h("p", {}, managed() ? [h("b", {}, `${kube.namespace}/${kube.name}`), " · rules that change its copies, shards or volumes. Click an underlined word: last week replays at once."] : "Rules that change a cluster's copies, shards or volumes. Click an underlined word: last week replays at once.")),
      h("div", { class: "tools" }, tools)));
  }

  /** What the operator's autoscaler says now: its reading, a suggestion waiting, why it waits, and its latest decisions. */
  function drawLive() {
    if (!managed()) {
      clear(live, h("div", { class: "as-note" }, icon("info"),
        h("div", {}, isCoordinator() ? "This cluster is not run by the Kubernetes operator: replay rules here on its load; they run once the operator runs it (spec.autoscale of its StrataCluster)." : "This server is not a cluster: the replay reads its own load, and rules run on a cluster the Kubernetes operator runs.")));
      return;
    }
    const st = status(), a = spec().autoscale;
    if (!st && !a) { clear(live); return; }
    const mode = a?.mode || "off";
    const sg = st?.suggestion;
    const parts = [
      h("div", { class: "as-live-n" }, h("span", { class: "small dim" }, "Busiest shard now"), h("b", {}, st?.busy == null ? "–" : pct(st.busy)), h("span", { class: "small dim" }, "of its cores, over a minute")),
      h("div", { class: "as-live-n" }, h("span", { class: "small dim" }, "Fullest volume"), h("b", {}, st?.full == null ? "–" : pct(st.full)), h("span", { class: "small dim" }, `${spec().replicas ?? "?"} copies × ${spec().shards ?? "?"} shards, ${spec().storage?.size || "10Gi"} each`)),
    ];
    const say = h("div", { class: "as-live-t" },
      h("div", { class: "row", style: { gap: "8px" } }, pill(mode === "on" ? "ok" : mode === "suggest" ? "info" : "disabled", mode === "on" ? "acting" : mode === "suggest" ? "asking a person" : "off"),
        st?.busySinceMs ? h("span", { class: "small" }, `over the limit since ${when(st.busySinceMs)} UTC`) : null,
        st?.idleSinceMs && !st?.busySinceMs ? h("span", { class: "small dim" }, `quiet since ${when(st.idleSinceMs)} UTC`) : null),
      sg ? h("div", { class: "as-sg" }, icon("bolt"), h("div", { class: "stack", style: { gap: "6px", flex: "1 1 260px" } }, h("b", {}, "The rules suggest a change"), h("span", {}, sg.text), h("span", { class: "small dim" }, `${fmt.ago(sg.atMs)}; it stands while its reason does.`)),
        h("div", { class: "row", style: { gap: "6px" } }, h("button", { class: "btn sm primary", disabled: busy, onclick: () => apply(sg) }, icon("check"), "Apply"), h("button", { class: "btn sm", disabled: busy, onclick: dismiss }, "Dismiss"))) : null,
      st?.waiting ? h("div", { class: "small dim" }, st.waiting) : !sg ? h("div", { class: "small dim" }, "No rule fires now.") : null);
    const log = (st?.log || []).slice(-4).reverse();
    clear(live, h("div", { class: "as-live" }, parts, say,
      log.length ? h("div", { class: "as-live-log" }, log.map((e) => h("div", { class: "as-ev" }, h("span", { class: "dim" }, `${when(e.atMs)}`), h("span", {}, e.text)))) : null));
  }

  /** What a rule did in the replay, in a line. */
  function didOf(id) {
    if (!replay) return "";
    const ev = replay.events || [], pts = replay.points || [], stepMin = (replay.stepMs || 600e3) / MIN;
    // A change by its rule, made or (asking a person) suggested.
    const made = ev.filter((e) => e.rule === id && e.kind !== "applied");
    const k = made.length;
    if (id === "grow") return k ? `In the replay: ${plural(k, "time")}, up to ${copies(Math.max(...pts.map((p) => p.replicas)))}.` : "In the replay: never.";
    if (id === "shrink") return k ? `In the replay: ${plural(k, "time")}, down to ${copies(Math.min(...pts.map((p) => p.replicas)))}.` : "In the replay: never.";
    if (id === "volumes") return k ? `In the replay: ${plural(k, "time")}.` : "In the replay: never.";
    const held = pts.filter((p) => p.waiting && (id === "freeze" ? /frozen/.test(p.waiting) : /have not passed/.test(p.waiting))).length;
    return held ? `In the replay it held a change back for ${hours((held * stepMin) / 60)}.` : "In the replay it held nothing back.";
  }

  function drawRules() {
    if (!draft) return;
    const rows = RULES.map((r) => {
      const on = isOn(r.id), v = valueOf(r.id);
      const words = r.parts(v).map((p) => {
        if (typeof p === "string") return h("span", {}, p);
        const id = `${r.id}.${p.key}`, open = editing === id;
        return h("button", { type: "button", class: "as-word" + (open ? " open" : ""), disabled: !on, "aria-expanded": String(open), title: `Change: ${p.show(p.value)}`, onclick: () => { editing = open ? null : id; drawRules(); } }, p.show(p.value));
      });
      const opened = editing?.startsWith(r.id + ".") ? r.parts(v).find((p) => typeof p !== "string" && `${r.id}.${p.key}` === editing) : null;
      return h("div", { class: "as-rule" + (on ? "" : " off") },
        h("div", { class: "as-rule-r" },
          h("span", { class: "as-kind " + r.kind }, r.kind),
          h("p", { class: "as-text" }, words),
          r.always ? h("span") : h("label", { class: "switch", title: on ? `Switch "${r.label}" off` : `Switch "${r.label}" on` }, h("input", { type: "checkbox", checked: on, "aria-label": r.label, onchange: (e) => switchRule(r.id, e.target.checked) }), h("span"))),
        opened ? h("div", { class: "as-choices", role: "group", "aria-label": `Choices for ${r.label}` }, opened.choices.map((c) => h("button", { type: "button", class: "as-choice", "aria-pressed": String(same(c, opened.value)), onclick: () => set(r.id, opened.key, c) }, opened.show(c)))) : null,
        on ? h("div", { class: "as-did small dim" }, replaying && !replay ? "Replaying…" : didOf(r.id)) : h("div", { class: "as-did small dim" }, "Off."));
    });
    clear(left, card({
      title: "The rules", sub: "in this order; a guard holds the others back",
      body: h("div", { class: "as-rules" }, rows, h("div", { class: "small dim" }, "Busy is the share of its cores a copy uses, averaged over the busiest shard's copies. Times are UTC.")),
    }));
  }

  // ------------------------------------------------------------ the replay
  function drawReplay() {
    if (!draft) return;
    if (replayErr && !replay) { clear(right, card({ title: "Last week, replayed", body: failure(replayErr, "the replay") })); return; }
    if (!replay) { clear(right, card({ title: "Last week, replayed", body: h("div", { class: "dim small" }, "Replaying the week…") })); return; }
    const pts = replay.points || [];
    if (pts.length < 2) {
      clear(right, card({ title: "Last week, replayed", body: empty("clock", "No load kept yet", "Every server keeps a row a minute of how busy it was (its log store's load table): the replay grows to a week as they run. A server with --log-store off keeps none.") }));
      return;
    }
    const sum = replay.summary, sizes = replay.sizes, stepMs = replay.stepMs || 600e3;
    const F = fixed || sizes.replicas;
    const fx = (p) => (p.fixed * sizes.replicas) / F;
    const sat = sum.saturated || 0.85;
    const t0 = pts[0].atMs, t1 = pts[pts.length - 1].atMs + stepMs, span = t1 - t0;
    const fixedOver = pts.filter((p) => fx(p) > sat).length * stepMs / MIN;
    const fixedHours = (F * sizes.shards * span) / 3600e3;
    const kept = span / 3600e3;
    const kpis = [
      { label: "Changes", value: fmt.n(sum.changes), sub: draft.mode === "suggest" ? `each waited ${sum.changes ? mins(Math.round(sum.waitedMinutes / sum.changes)) : "–"} for a person` : `ends at ${copies(sum.end.replicas)} × ${plural(sum.end.shards, "shard")}` },
      { label: `Over ${pct(sat)} busy`, value: hours(sum.overMinutes / 60), sub: `fixed at ${copies(F)}: ${hours(fixedOver / 60)}`, tone: sum.overMinutes > fixedOver ? "bad" : sum.overMinutes < fixedOver ? "good" : "" },
      { label: "Copy-hours", value: fmt.n(sum.copyHours, 0), sub: `fixed: ${fmt.n(fixedHours, 0)} (${sum.copyHours <= fixedHours ? "−" : "+"}${fmt.n(Math.abs(1 - sum.copyHours / Math.max(1, fixedHours)) * 100, 0)}%)`, tone: sum.copyHours < fixedHours ? "good" : "" },
      { label: "Load kept", value: hours(kept), sub: `from ${(replay.nodes || []).filter((n) => !n.error).length} servers` },
    ];
    const kpiEl = h("div", { class: "as-kpis" }, kpis.map((k) => h("div", { class: "as-kpi" }, h("span", { class: "small dim" }, k.label), h("b", { class: k.tone || "" }, k.value), h("span", { class: "small dim" }, k.sub))));

    // The chart: how busy the copies would have been.
    const W = 1000, H = 220;
    const top = Math.max(1, ...pts.map((p) => Math.max(p.busy, fx(p)))) * 1.05;
    const x = (t) => ((t - t0) / span) * W, y = (v) => H - (v / top) * H;
    const line = (f) => pts.map((p, i) => `${i ? "L" : "M"}${x(p.atMs + stepMs / 2).toFixed(1)},${y(f(p)).toFixed(1)}`).join("");
    const bands = (test, color) => pts.filter(test).map((p) => s("rect", { x: x(p.atMs), y: 0, width: Math.max(0.5, x(p.atMs + stepMs) - x(p.atMs)), height: H, fill: color }));
    const days = [...new Set(pts.map((p) => dayOf(p.atMs)))];
    const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "as-chart", preserveAspectRatio: "none", role: "img", "aria-label": `How busy the copies would have been over ${hours(kept)}: ${sum.changes} changes` },
      days.filter((d, i) => i % 2 === 1).map((d) => s("rect", { x: x(Math.max(t0, d * 86400e3)), y: 0, width: x(Math.min(t1, (d + 1) * 86400e3)) - x(Math.max(t0, d * 86400e3)), height: H, fill: "var(--surface-2)" })),
      bands((p) => p.frozen, "color-mix(in srgb, var(--s5) 14%, transparent)"),
      bands((p) => p.busy > sat, "color-mix(in srgb, var(--crit) 30%, transparent)"),
      s("line", { x1: 0, x2: W, y1: y(sat), y2: y(sat), stroke: "var(--crit)", "stroke-width": 1, "stroke-dasharray": "3 3", "vector-effect": "non-scaling-stroke" }),
      s("path", { d: line(fx), fill: "none", stroke: "var(--muted)", "stroke-width": 1.5, "stroke-dasharray": "6 4", "vector-effect": "non-scaling-stroke" }),
      s("path", { d: line((p) => p.busy), fill: "none", stroke: "var(--s1)", "stroke-width": 2, "stroke-linejoin": "round", "vector-effect": "non-scaling-stroke" }));
    const GLYPH = { grow: "▲", shrink: "▼", volumes: "◆", suggested: "?", applied: "✓" };
    const marks = (replay.events || []).filter((e) => GLYPH[e.kind]).map((e) => tipOn(h("span", { class: "as-mark " + e.kind, style: { left: `${(x(e.atMs) / W) * 100}%` }, tabindex: 0, "aria-label": `${when(e.atMs)}: ${e.text}` }, GLYPH[e.kind]), () => h("div", {}, h("b", {}, `${when(e.atMs)} UTC`), h("div", {}, e.text))));
    const hover = h("div", { class: "as-hover" });
    const plot = h("div", { class: "as-plot" }, svg, marks, h("span", { class: "as-y small dim" }, pct(top)), h("span", { class: "as-y as-y-sat small", style: { top: `${(y(sat) / H) * 100}%` } }, pct(sat)), hover);
    plot.addEventListener("pointermove", (ev) => {
      const r = plot.getBoundingClientRect(), t = t0 + ((ev.clientX - r.left) / r.width) * span;
      const p = pts.reduce((b, q) => (Math.abs(q.atMs + stepMs / 2 - t) < Math.abs(b.atMs + stepMs / 2 - t) ? q : b), pts[0]);
      hover.style.left = `${(x(p.atMs + stepMs / 2) / W) * 100}%`; hover.style.display = "block";
      if (ev.target.closest(".as-mark")) return;
      showTip(ev.clientX, ev.clientY, h("div", { class: "stack", style: { gap: "2px" } },
        h("b", {}, `${when(p.atMs)} UTC`),
        h("div", {}, `with the rules: ${pct(p.busy)} busy, ${copies(p.replicas)}${p.serving < p.replicas ? ` (${p.serving} serving, the others catching up)` : ""}${p.shards !== sizes.shards ? `, ${p.shards} shards` : ""}`),
        h("div", { class: "dim" }, `fixed at ${copies(F)}: ${pct(fx(p))} busy`),
        p.full != null ? h("div", { class: "dim" }, `fullest volume ${pct(p.full)}`) : null,
        p.waiting ? h("div", { class: "small", style: { maxWidth: "320px" } }, p.waiting) : null));
    });
    plot.addEventListener("pointerleave", () => { hover.style.display = "none"; hideTip(); });

    // The days, each with its events.
    const evDays = {};
    for (const e of replay.events || []) (evDays[dayOf(e.atMs)] ||= []).push(e);
    if (day == null || !days.includes(day)) day = days.reduce((b, d) => ((evDays[d] || []).length > (evDays[b] || []).length ? d : b), days[days.length - 1]);
    const dayBtns = h("div", { class: "as-days", role: "group", "aria-label": "Pick a day to read what happened" }, days.map((d) => {
      const n = (evDays[d] || []).filter((e) => e.kind !== "caught-up" && e.kind !== "freeze").length;
      const a = x(Math.max(t0, d * 86400e3)), b = x(Math.min(t1, (d + 1) * 86400e3));
      return h("button", { type: "button", class: "as-day", "aria-pressed": String(d === day), style: { flex: `${Math.max(1, b - a)} 1 0` }, title: `${dayLabel(d)}: ${plural(n, "change")}`, onclick: () => { day = d; drawReplay(); } }, dayLabel(d), n ? h("span", { class: "as-dot" }, n) : null);
    }));
    const options = [...new Set([Math.max(1, sizes.replicas - 1), sizes.replicas, sizes.replicas + 1, draft.grow?.max].filter(Boolean))].sort((a, b) => a - b);
    const legend = h("div", { class: "as-legend small dim" },
      h("span", {}, h("i", { class: "as-sw", style: { background: "var(--s1)" } }), "busy, with the rules"),
      h("span", {}, h("i", { class: "as-sw dash" }), `busy at a fixed ${copies(F)}`),
      h("span", {}, h("i", { class: "as-sw box", style: { background: "color-mix(in srgb, var(--crit) 45%, transparent)" } }), `over ${pct(sat)}`),
      draft.freeze.length ? h("span", {}, h("i", { class: "as-sw box", style: { background: "color-mix(in srgb, var(--s5) 30%, transparent)" } }), "no changes allowed") : null,
      h("span", {}, "▲ ▼ ◆ a change", draft.mode === "suggest" ? ", ? asked, ✓ applied" : ""));

    // Copies of each shard, and the fullest volume.
    const H2 = 40;
    const lo = Math.min(...pts.map((p) => p.replicas)), hi = Math.max(...pts.map((p) => p.replicas), F);
    const yc = (v) => H2 - 2 - ((v - 0) / Math.max(1, hi)) * (H2 - 6);
    const area = `M0,${H2}` + pts.map((p) => `L${x(p.atMs).toFixed(1)},${yc(p.replicas).toFixed(1)}L${x(p.atMs + stepMs).toFixed(1)},${yc(p.replicas).toFixed(1)}`).join("") + `L${W},${H2}Z`;
    const copiesEl = h("div", { class: "as-strip" }, h("span", { class: "small dim" }, "Copies of each shard"),
      h("div", { class: "as-strip-p" }, s("svg", { viewBox: `0 0 ${W} ${H2}`, preserveAspectRatio: "none", "aria-hidden": "true" },
        s("line", { x1: 0, x2: W, y1: yc(F), y2: yc(F), stroke: "var(--muted)", "stroke-dasharray": "6 4", "vector-effect": "non-scaling-stroke" }),
        s("path", { d: area, fill: "color-mix(in srgb, var(--s1) 22%, transparent)", stroke: "var(--s1)", "stroke-width": 1.5, "vector-effect": "non-scaling-stroke" })),
        h("span", { class: "as-strip-v small dim" }, lo === hi ? copies(lo) : `${lo} to ${hi}`)));
    const fulls = pts.filter((p) => p.full != null);
    const lim = draft.volumes?.fullAbove;
    const yf = (v) => H2 - 2 - v * (H2 - 6);
    const diskEl = fulls.length > 1 ? h("div", { class: "as-strip" }, h("span", { class: "small dim" }, "Fullest volume"),
      h("div", { class: "as-strip-p" }, s("svg", { viewBox: `0 0 ${W} ${H2}`, preserveAspectRatio: "none", "aria-hidden": "true" },
        lim ? s("line", { x1: 0, x2: W, y1: yf(lim), y2: yf(lim), stroke: "var(--warn)", "stroke-dasharray": "4 3", "vector-effect": "non-scaling-stroke" }) : null,
        s("path", { d: fulls.map((p, i) => `${i ? "L" : "M"}${x(p.atMs).toFixed(1)},${yf(p.full).toFixed(1)}`).join(""), fill: "none", stroke: "var(--warn)", "stroke-width": 1.5, "vector-effect": "non-scaling-stroke" })),
        h("span", { class: "as-strip-v small dim" }, pct(fulls[fulls.length - 1].full)))) : null;

    const chartCard = card({
      cls: "as-replay",
      title: "Last week, replayed", sub: `${hours(kept)} of load from the servers' log stores, ${replay.managed ? "from the StrataCluster's size" : "from the cluster's size"}: ${copies(sizes.replicas)} × ${plural(sizes.shards, "shard")}`,
      tools: [h("span", { class: "small dim" }, "Compare with a fixed"), seg(options.map((n) => [n, copies(n)]), F, (v) => { fixed = v; drawReplay(); }, { "aria-label": "Compare with a fixed size" }), replaying ? h("span", { class: "small dim" }, "…") : null],
      body: h("div", { class: "stack", style: { gap: "10px" } }, kpiEl, plot, dayBtns, legend, copiesEl, diskEl),
    });
    const evs = (evDays[day] || []);
    const dayCard = card({
      title: dayLabel(day), sub: "UTC",
      body: evs.length ? h("div", { class: "as-evs" }, evs.map((e) => h("div", { class: "as-ev " + e.kind }, h("span", { class: "dim" }, when(e.atMs).slice(4)), h("span", {}, cap(e.text))))) : h("div", { class: "small dim" }, "The rules did nothing this day."),
    });
    clear(right, chartCard, dayCard);
  }
  const cap = (t) => t.charAt(0).toUpperCase() + t.slice(1) + (/[.)]$/.test(t) ? "" : ".");

  load();
  const t = setInterval(() => { if (alive) load(); }, 15000);
  return () => { alive = false; clearInterval(t); hideTip(); };
}
