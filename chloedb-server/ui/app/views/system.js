// Settings: how every server of the cluster runs, the upkeep its shards
// need, and this server. On Kubernetes the settings are the StrataCluster's
// spec, edited a setting at a time, each saying what it does at its value;
// the coordinator plans the change and one Apply (with a reason) hands it
// to the operator. On machines of your own they are the flags each server
// is started with: the page says what it can read of them and how to
// change one. Upkeep says in words how the replicas compare and how full
// the disks are, against thresholds; a copy's own operations (verify,
// checkpoint, make primary, remove) are on the Cluster canvas. A server
// that is not a cluster keeps its operations here (check, vacuum,
// checkpoint, verify its replicas, a snapshot lease), each answering in
// words. The process, its limits, the configuration and the raw
// Prometheus metrics are under This server.

import { h, s, icon, clear, fmt, card, api, every, toast, modal, drawer, empty, failure, debounce, copy, showTip, hideTip, bus, ApiError, seg, toggle, store, confirm, can, needs, link, copyName } from "../core.js";
import * as core from "../core.js";
import { SERIES, sparkline } from "../charts.js";
import { state, refreshSystem, refreshCluster, last, isCoordinator, isProduction, clusterName } from "../data.js";
import { SETTINGS, TAKES, value, assign, shown, same, clone, full, patchOf, live, after, steps, quantity, line, words, getPath, argOf, yaml, lineDiff, plural, fullIn } from "../spec.js";

let active = null;

export default {
  id: "system", title: "Settings", icon: "system", group: "Change",
  keywords: "settings flags spec statefulset write acks failover hedge timeout slow limits resources memory cores volume storage token coordinators upkeep verify compare replicas checkpoint backup vacuum check snapshot lease reset statistics process limits segments merges config metrics prometheus disk",
  render, onParams: (r) => active?.onParams(r),
};

const kv = (pairs) => h("dl", { class: "kv" }, pairs.filter(Boolean).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v ?? "–")]));
const cap = (t) => (!t || /^(s\d+-\d+|[a-z]+:\/\/|[a-z][\w-]*\d[\w-]*\b|\S+:\d+\b)/.test(t) ? t : t[0].toUpperCase() + t.slice(1));
const narrow = () => (typeof core.isNarrow === "function" ? core.isNarrow() : matchMedia("(max-width: 900px)").matches);
const canAdmin = () => can("admin");
const needsAdmin = needs("admin");
const ask = (o) => confirm({ reason: isProduction() ? "Why: kept in the audit log with the change" : false, ...o });
const errText = (e) => (e instanceof ApiError && e.status === 403 ? "it needs an admin key" : e?.message || String(e));

// ---------------------------------------------------------------- thresholds
/** Where a share turns amber and red: a disk needs room for merges, memory for the queries' working sets. */
const DISK = { warn: 0.8, bad: 0.9, why: { 1: "over 80%: plan more room soon", 2: "over 90%: a merge needs room to write the new segment beside the old ones, and may fail" } };
const MEM = { warn: 0.75, bad: 0.9, why: { 1: "over 75% of the limit", 2: "over 90% of the limit: the server may be killed for memory" } };
const lvl = (v, t) => (v == null ? 0 : v >= t.bad ? 2 : v >= t.warn ? 1 : 0);
const tone = (l) => (l === 2 ? "cc-bad" : l === 1 ? "cc-warn" : "");

/** A bar with the thresholds marked, the share in words beside it. */
function meter(v, t, label) {
  const l = lvl(v, t);
  return h("div", { class: "cc-meter-w" },
    h("div", { class: "row nw between small" }, h("span", { class: "dim" }, label), h("b", { class: tone(l) }, v == null ? "–" : fmt.pct(v, v > 0 && v < 0.1 ? 1 : 0))),
    h("div", { class: "cc-meter", role: "img", "aria-label": `${label}: ${v == null ? "unknown" : fmt.pct(v, 0)}` },
      h("i", { class: "cc-meter-f l" + l, style: { width: `${Math.max(0, Math.min(1, v ?? 0)) * 100}%` } }),
      h("i", { class: "cc-meter-t", style: { left: `${t.warn * 100}%` }, title: `amber from ${fmt.pct(t.warn, 0)}` }),
      h("i", { class: "cc-meter-t bad", style: { left: `${t.bad * 100}%` }, title: `red from ${fmt.pct(t.bad, 0)}` })),
    l && t.why?.[l] ? h("div", { class: "small " + tone(l) }, cap(t.why[l]) + ".") : null);
}

/** The last hour of one figure of this server's points: a sparkline and the change in words. */
function trend(key, { show = fmt.bytes, ratio = false } = {}) {
  const pts = (state.points || []).filter((p) => p[key] != null && Number.isFinite(p[key]));
  if (pts.length < 8) return null;
  const a = pts[0], b = pts[pts.length - 1];
  const t0 = a.t > 1e12 ? a.t : a.t * 1000, t1 = b.t > 1e12 ? b.t : b.t * 1000;
  const hrs = (t1 - t0) / 3600e3;
  if (!(hrs > 0.02)) return null;
  const d = b[key] - a[key];
  const rel = a[key] ? Math.abs(d) / a[key] : 0;
  const span = hrs >= 0.95 ? "the last hour" : `the last ${Math.max(1, Math.round(hrs * 60))} min`;
  const said = rel < 0.01 && Math.abs(d) < (ratio ? 0.005 : 1) ? `steady over ${span}` : `${d > 0 ? "up" : "down"} ${ratio ? fmt.pct(Math.abs(d), 1) : show(Math.abs(d))} over ${span}`;
  return h("div", { class: "row nw cc-trend" }, sparkline(pts.map((p) => p[key]), { width: 96, height: 22, color: "var(--s1)" }), h("span", { class: "small dim" }, said));
}

// ---------------------------------------------------------------- verify, in words
/**
 * What a primary's comparison of its replicas found
 * (`POST /v1/admin/verify` or the coordinator's per-copy proxy), in words:
 * {tone, summary, lines}.
 */
export function verifyWords(j, name = (u) => copyName(u)) {
  const rs = j?.replicas || j?.results || [];
  if (!rs.length) return { tone: "ok", summary: "No replica to compare: the primary ships to none.", lines: [] };
  const lines = rs.map((r) => {
    const n = name(r.url);
    const differ = r.differing_slots ?? (r.differs ? 1 : 0);
    if (r.error) return { tone: "bad", text: `${n}: the comparison failed: ${r.error}` };
    if (r.in_sync === false) return { tone: "warn", text: `${n} is not in sync yet, so it was not compared: it is once it has caught up` };
    if (r.compared === false) return { tone: "warn", text: `${n} did not answer in time, so it was not compared` };
    if (differ > 0) return { tone: "bad", text: `${n} differed in ${plural(differ, "slot")}${r.first_differing_slot != null ? ` (the first is slot ${fmt.n(r.first_differing_slot)})` : ""}: it is sent a copy of the primary` };
    // An answer from an earlier comparison, not this one: say how old it is.
    if (r.ms_ago > 60000) return { tone: "warn", old: true, text: `${n} was not compared now: its last comparison, ${fmt.dur(r.ms_ago / 1000)} ago${r.seq_no != null ? ` up to write ${fmt.n(r.seq_no)}` : ""}, found the same rows. Compare again to be sure` };
    return { tone: "ok", text: `${n} holds the same rows${r.seq_no ? ` (up to write ${fmt.n(r.seq_no)})` : ""}` };
  });
  const bad = lines.filter((l) => l.tone === "bad").length, warn = lines.filter((l) => l.tone === "warn").length;
  const summary = bad ? `${plural(bad, "replica")} of ${rs.length} differed or failed` : warn ? `${fmt.n(rs.length - warn)} of ${plural(rs.length, "replica")} compared now: the same rows` : rs.length === 1 ? "The replica holds the same rows as its primary" : `All ${rs.length} replicas hold the same rows as their primary`;
  return { tone: bad ? "bad" : warn ? "warn" : "ok", summary, lines };
}

/** What the cluster's primaries say of their replicas' comparisons (each primary compares on its own every --verify-every-s). */
function comparisons(c) {
  const out = { shards: 0, primaries: [], every: null, compared: 0, replicas: 0, newest: null, oldest: null, differ: [], notSynced: [], failed: [], noPrimary: [], never: [] };
  (c?.shards || []).forEach((sh, i) => {
    out.shards++;
    const reps = sh.replicas || [];
    const prim = reps.filter((r) => r.status !== "down" && r.takes_writes).sort((a, b) => (b.term ?? 0) - (a.term ?? 0))[0];
    const rep = prim?.detail?.health?.replication;
    if (!prim) { out.noPrimary.push(i); return; }
    out.primaries.push({ shard: sh.shard ?? i, url: prim.url, replicas: (rep?.replicas || []).length });
    if (rep?.role !== "primary") return;
    out.every ??= rep.verify_every_s;
    for (const f of rep.replicas || []) {
      out.replicas++;
      const v = f.verified || {};
      if (f.in_sync === false) out.notSynced.push({ shard: i, url: f.url });
      if (v.error) out.failed.push({ shard: i, url: f.url, error: v.error });
      if (v.differing_slots > 0) out.differ.push({ shard: i, url: f.url, n: v.differing_slots, first: v.first_differing_slot, ago: v.ms_ago });
      if (v.ms_ago != null) {
        out.compared++;
        out.newest = out.newest == null ? v.ms_ago : Math.min(out.newest, v.ms_ago);
        out.oldest = out.oldest == null ? v.ms_ago : Math.max(out.oldest, v.ms_ago);
      } else if (!f.verifications && f.in_sync !== false) out.never.push({ shard: i, url: f.url });
    }
  });
  return out;
}

// What the operations answered, kept while the page is open (by server).
const results = new Map();

// ---------------------------------------------------------------- the view
function render(page, r) {
  const coord = () => isCoordinator();
  const tabsFor = () => (coord() ? [["settings", "Settings"], ["upkeep", "Upkeep"], ["server", "This coordinator"]] : [["upkeep", "Upkeep"], ["server", "This server"]]);
  const pick = (params) => { const t = params?.get?.("tab"); return tabsFor().some(([k]) => k === t) ? t : tabsFor()[0][0]; };
  let tab = pick(r?.params);
  let alive = true;

  const head = h("div"), body = h("div", { class: "cc-set-page" });
  page.append(head, body);

  function writeTab() {
    const q = new URLSearchParams();
    if (tab !== tabsFor()[0][0]) q.set("tab", tab);
    try { history.replaceState(null, "", location.pathname + location.search + "#/system" + (q.toString() ? "?" + q : "")); } catch { /* old browser */ }
  }
  function drawHead() {
    const name = clusterName();
    clear(head, h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, "Settings"),
        h("p", {}, coord()
          ? [`How every server${name ? ` of ${name}` : ""} runs, the upkeep its shards need, and this coordinator. `, "Shards and copies change on ", h("a", { href: link("cluster") }, "Cluster"), "; versions on ", h("a", { href: link("rollout") }, "Rollouts"), "."]
          : "This server: the upkeep its database needs, its process and its limits.")),
      h("div", { class: "tools" }, seg(tabsFor(), tab, (v) => { tab = v; writeTab(); draw(); }, { "aria-label": "Part of the settings" }))));
  }
  function draw() {
    if (!alive) return;
    drawHead();
    body.classList.remove("has-bar");
    if (tab === "settings") fleet.draw();
    else if (tab === "upkeep") (coord() ? fleetUpkeep : serverUpkeep).draw();
    else serverTab.draw();
  }

  // ============================================================ every server (fleet settings)
  const fleet = (() => {
    let kube = null, kubeErr = null, base = null, draft = null;
    let plan = null, planErr = null, planFor = null, planning = false, applying = false;
    let said = [], watching = false, filter = "", onlyChanged = false, reviewOpen = null;
    const managed = () => !!kube?.managed;
    const key = () => `settings.draft.${kube.namespace}/${kube.name}`;
    const SHOWN = SETTINGS.filter((st) => st.g !== "Version");
    const ctx = () => { const L = live(state.cluster); const d = draft || {}; const S = d.shards ?? L.S, R = d.replicas ?? L.R; return { L, d, S, R, A: after(L, S, R) }; };
    const changedList = () => (draft && base ? SHOWN.filter((st) => !same(value(draft, st), value(base, st))) : []);

    async function load() {
      try { kube = await api("/v1/kube"); kubeErr = null; } catch (e) { kube = { managed: false }; kubeErr = e; }
      if (managed()) {
        const spec = full(kube.spec);
        if (!base || (!same(spec, base) && !changedList().length)) {
          base = spec;
          const kept = store.get(key(), null);
          draft = kept && same(kept.base, base) && !same(kept.draft, base) ? kept.draft : clone(base);
          replan();
        }
      }
      if (tab === "settings") draw1();
    }
    function set(f) {
      const d = clone(draft);
      f(d);
      if (same(d, draft)) return;
      draft = d;
      store.set(key(), same(draft, base) ? null : { base, draft });
      draw1(); replan();
    }
    function discard() { draft = clone(base); store.set(key(), null); plan = null; planFor = null; draw1(); }

    const replan = debounce(async () => {
      if (!managed() || !draft) return;
      const patch = patchOf(base, draft);
      if (!Object.keys(patch).length) { plan = null; planErr = null; planFor = null; drawReview(); return; }
      const want = JSON.stringify(patch);
      if (want === planFor && plan) return;
      planning = true; planFor = want; drawReview();
      try { const p = await api("/v1/admin/kube/plan", { body: { spec: patch } }); if (planFor === want) { plan = p; planErr = null; } }
      catch (e) { if (planFor === want) { plan = null; planErr = errText(e); } }
      planning = false;
      drawReview();
    }, 300);

    async function apply() {
      const patch = patchOf(base, draft);
      const list = changedList();
      const ok = await ask({
        title: `Change ${plural(list.length, "setting")} of ${kube.namespace}/${kube.name}`,
        body: h("div", { class: "stack", style: { gap: "8px" } },
          h("ul", { class: "cc-set-chg" }, list.map((st) => h("li", {}, h("b", {}, st.label), h("span", {}, `${shown(st, value(base, st)) ?? "unset"} → ${shown(st, value(draft, st)) ?? "unset"}`), h("span", { class: "small dim" }, TAKES[st.takes] || st.takes)))),
          plan?.steps?.length ? h("div", {}, h("div", { class: "small dim" }, "The operator then:"), h("ol", { class: "cc-set-steps" }, plan.steps.map((x) => h("li", {}, cap(x))))) : null,
          (plan?.warnings || []).length ? h("div", { class: "small cc-warn" }, "Mind: " + plan.warnings.join("; ") + ".") : null),
        ok: `Apply ${plural(list.length, "change")}`,
      });
      if (!ok) return;
      applying = true; drawReview();
      try {
        const j = await api("/v1/admin/kube", { body: { spec: patch }, reason: ok.reason });
        toast("The operator has the new settings", "ok");
        store.set(key(), null);
        if (j?.managed) kube = j;
        base = full(kube.spec || {});
        if (!same(base, draft)) base = clone(draft);
        draft = clone(base);
        plan = null; planFor = null; said = []; watching = true;
        reviewOpen?.close?.(); reviewOpen = null;
        watch();
      } catch (e) { toast("Not applied: " + errText(e), "bad", 8000); }
      applying = false;
      draw1();
    }
    let stopWatch = null;
    function watch() {
      stopWatch?.();
      stopWatch = every(2000, async () => {
        let k; try { k = await api("/v1/kube"); } catch { return; }
        kube = k;
        const m = k.status?.message;
        if (m && said[said.length - 1] !== m) said.push(m);
        if (k.settled && k.status?.phase === "Ready" && said.length) { watching = false; stopWatch?.(); stopWatch = null; refreshCluster(); }
        if (tab === "settings") drawReview();
      });
    }

    // ---------------------------------------------------------- drawing
    let listBox = null, reviewBox = null, barBox = null;
    function draw1() {
      if (!state.cluster && !kube) { clear(body, empty("system", "Asking the coordinator…")); return; }
      if (!kube) { clear(body, empty("system", "Asking the coordinator…")); return; }
      if (!managed()) return drawOwn();
      listBox = h("div", { class: "cc-set-list" }); reviewBox = h("div", { class: "cc-set-rev" }); barBox = h("div");
      clear(body, h("div", { class: "cc-set-main" }, listBox, narrow() ? null : reviewBox), narrow() ? barBox : null);
      body.classList.toggle("has-bar", narrow());
      drawList(); drawReview();
    }
    function drawList() {
      if (!listBox) return;
      const x = ctx(), d = draft;
      const groups = [...new Set(SHOWN.map((st) => st.g))];
      const q = filter.trim().toLowerCase();
      const out = [];
      let n = 0;
      for (const g of groups) {
        const rows = SHOWN.filter((st) => st.g === g).filter((st) => {
          if (onlyChanged && same(value(d, st), value(base, st))) return false;
          return !q || `${st.label} ${st.at} ${g}`.toLowerCase().includes(q);
        });
        if (!rows.length) continue;
        n += rows.length;
        out.push(h("div", { class: "cc-set-g" }, h("div", { class: "cc-set-gt" }, g), rows.map((st) => row(st, d, x))));
      }
      const img = kube.spec?.image || base.image;
      const find = h("input", { class: "input sm", type: "search", placeholder: "Find a setting", value: filter, "aria-label": "Find a setting", oninput: debounce((e) => { filter = e.target.value; drawList(); const f = listBox.querySelector("input[type=search]"); f?.focus(); f?.setSelectionRange(f.value.length, f.value.length); }, 120) });
      clear(listBox, card({
        title: "Every server", sub: `the StrataCluster ${kube.namespace}/${kube.name}`,
        tools: [find, h("button", { class: "btn sm" + (onlyChanged ? " primary" : ""), "aria-pressed": String(onlyChanged), onclick: () => { onlyChanged = !onlyChanged; drawList(); } }, "Only changed")],
        body: h("div", { class: "stack", style: { gap: "14px" } },
          n ? out : h("div", { class: "small muted" }, onlyChanged ? "Nothing is changed yet." : "No setting matches."),
          !q && !onlyChanged ? h("div", { class: "cc-set-g" }, h("div", { class: "cc-set-gt" }, "Version"),
            h("div", { class: "cc-set" }, h("div", { class: "cc-set-l" }, h("b", {}, "Image"), h("div", { class: "small dim" }, `Every server and coordinator runs ${img}. A new version rolls out one copy at a time, held by its gates.`)),
              h("div", { class: "cc-set-r" }, h("a", { class: "btn sm", href: link("rollout") }, "Change it in Rollouts", icon("right"))))) : null),
      }));
    }
    function row(st, d, x) {
      const v = value(d, st), was = value(base, st), changedNow = !same(v, was);
      const [where, rest] = st.at.split(/:(.*)/s);
      const set1 = (nv) => set((dd) => assign(dd, st, nv));
      const label = st.label.toLowerCase();
      let control;
      if (st.type === "seg") { control = seg(st.opts.map(([k, l]) => [k === undefined ? "" : k, l]), v ?? "", (nv) => set1(nv === "" ? undefined : nv), { "aria-label": st.label }); if (!canAdmin()) for (const btn of control.querySelectorAll("button")) btn.disabled = true; }
      else if (st.type === "toggle") { control = toggle(!!v, (on) => set1(on), v ? "on" : "off"); const i = control.querySelector("input"); if (i) i.disabled = !canAdmin(); }
      else if (st.type === "step") {
        const list = st.list, cur = v ?? st.def;
        const lo = st.grows ? Math.max(0, list.findIndex((q) => quantity(q) >= quantity(was))) : 0;
        const i = list.findIndex((q) => same(q, cur));
        control = h("div", { class: "row nw cc-set-step" },
          h("button", { class: "btn sm icon", "aria-label": "Less: " + label, title: st.grows && i <= lo ? "A volume only grows" : "", disabled: !canAdmin() || (i <= lo && i >= 0), onclick: () => set1(steps(list.slice(lo), cur, -1)) }, "−"),
          h("b", {}, shown(st, v)),
          h("button", { class: "btn sm icon", "aria-label": "More: " + label, disabled: !canAdmin() || i === list.length - 1, onclick: () => set1(steps(list, cur, 1)) }, "+"));
      } else if (st.type === "text") control = h("input", { class: "input sm cc-set-text", value: v ?? "", placeholder: st.placeholder, "aria-label": st.label, disabled: !canAdmin(), onchange: (e) => set1(e.target.value.trim() || undefined) });
      else if (st.type === "argv") control = h("input", { class: "input sm cc-set-text wide", value: line(v), placeholder: st.placeholder, "aria-label": st.label, disabled: !canAdmin(), onchange: (e) => set1(words(e.target.value)) });
      const inSpec = where === "spec" ? getPath(kube.spec || {}, rest) !== undefined : where === "argv" ? (kube.spec?.[rest] || []).length > 0 : argOf(base[where === "args" ? "args" : "coordinatorArgs"], rest, st.bool) !== undefined;
      const prov = changedNow ? `was ${shown(st, was) ?? "unset"}` : where === "spec" ? (inSpec ? "set in the spec" : "the operator's default") : where === "argv" ? (inSpec ? "set in the spec" : "none") : inSpec ? `set in ${where === "args" ? "args" : "coordinatorArgs"}` : "chloedb-server's default";
      const effect = st.effect?.(v ?? st.def, x);
      const warn = st.warn?.(v ?? st.def, x);
      return h("div", { class: "cc-set" + (changedNow ? " changed" : "") + (st.type === "argv" || st.type === "seg" ? " stacked" : "") },
        h("div", { class: "cc-set-l" },
          h("b", {}, st.label),
          effect ? h("div", { class: "small dim" }, effect) : null,
          warn ? h("div", { class: "small cc-warn" }, cap(warn) + ".") : null),
        h("div", { class: "cc-set-r" }, control,
          h("div", { class: "row cc-set-prov" },
            h("span", { class: "small " + (changedNow ? "cc-set-was" : "muted") }, prov),
            changedNow ? h("span", { class: "small muted" }, "· " + (TAKES[st.takes] || st.takes)) : null,
            changedNow ? h("button", { class: "btn sm icon ghost", title: "Put it back", "aria-label": "Put back: " + label, onclick: () => set((dd) => assign(dd, st, was)) }, icon("reset")) : null)));
    }

    function reviewParts() {
      const list = changedList();
      const st = kube.status || {};
      const progress = watching || said.length ? h("div", { class: "stack", style: { gap: "6px" } },
        h("div", { class: "cc-set-gt" }, watching ? "The operator is on it" : "The operator made the change"),
        said.length ? h("ol", { class: "cc-set-steps done" }, said.map((m, i) => h("li", { class: i === said.length - 1 && watching ? "now" : "" }, cap(m)))) : h("div", { class: "small muted" }, "Waiting for the operator to read the new spec…")) : null;
      if (!list.length) {
        return [
          h("div", { class: "small dim" }, "Nothing changed. Each setting says what it does at its value now: change one and the coordinator plans it here, before anything applies."),
          st.phase && st.phase !== "Ready" && !progress ? h("div", { class: "small" }, h("b", {}, "The operator now: "), cap(st.message || st.phase)) : null,
          progress];
      }
      const patch = patchOf(base, draft);
      const lines = lineDiff(yaml(base), yaml(draft));
      const keep = lines.map((_, i) => lines.slice(Math.max(0, i - 1), i + 2).some(([m]) => m !== " "));
      const diff = [];
      let skipped = 0;
      lines.forEach(([m, t], i) => {
        if (!keep[i]) { skipped++; return; }
        if (skipped) { diff.push(h("div", { class: "cc-dl skip" }, `  … ${skipped} more`)); skipped = 0; }
        diff.push(h("div", { class: "cc-dl " + (m === "+" ? "add" : m === "-" ? "del" : "") }, (m === " " ? "  " : m + " ") + t));
      });
      const takes = [...new Set(list.map((x) => TAKES[x.takes] || x.takes))];
      const blocked = !!plan?.refused || !!planErr;
      return [
        h("ul", { class: "cc-set-chg" }, list.map((x) => h("li", {}, h("b", {}, x.label), h("span", {}, `${shown(x, value(base, x)) ?? "unset"} → ${shown(x, value(draft, x)) ?? "unset"}`)))),
        h("div", { class: "small dim" }, `It ${takes.join(", and ")}.`),
        h("div", { class: "cc-set-gt" }, "The operator's plan"),
        planning && !plan ? h("div", { class: "small muted" }, "Asking the coordinator…")
          : planErr ? h("div", { class: "small cc-bad" }, "Not planned: " + planErr)
          : plan ? h("div", { class: "stack", style: { gap: "6px" } },
            (plan.steps || []).length ? h("ol", { class: "cc-set-steps" }, plan.steps.map((x) => h("li", {}, cap(x)))) : h("div", { class: "small dim" }, "The operator restarts what the change takes, one at a time, while the cluster serves."),
            (plan.warnings || []).map((w) => h("div", { class: "small cc-warn" }, cap(w) + ".")),
            plan.refused ? h("div", { class: "small cc-bad" }, "Refused: " + plan.refused) : null) : null,
        st.phase && st.phase !== "Ready" && !(plan?.warnings || []).length ? h("div", { class: "small" }, h("b", {}, "The operator now: "), cap(st.message || st.phase)) : null,
        h("details", { class: "cc-set-diff" }, h("summary", { class: "small" }, `The change to the spec (${plural(Object.keys(patch).length, "field")})`), h("pre", { class: "cc-diff" }, diff)),
        h("div", { class: "row", style: { gap: "6px" } },
          h("button", { class: "btn primary", disabled: !canAdmin() || !plan || blocked || planning || applying, title: canAdmin() ? "" : needsAdmin, onclick: apply }, applying ? "Applying…" : `Apply ${plural(list.length, "change")}…`),
          h("button", { class: "btn", disabled: applying, onclick: discard }, "Discard")),
        h("div", { class: "small muted" }, "The draft stays in this browser until you apply or discard it."),
        progress];
    }
    function drawReview() {
      if (!managed() || !listBox) return;
      const list = changedList();
      if (narrow()) {
        clear(barBox, list.length || watching ? h("div", { class: "cc-planbar cc-set-bar" },
          h("div", { class: "grow small" }, list.length ? h("b", {}, plural(list.length, "change")) : null, list.length ? ` · ${[...new Set(list.map((x) => TAKES[x.takes] || x.takes))][0]}` : watching ? "The operator is on it" : ""),
          h("button", { class: "btn sm primary", onclick: openReview }, list.length ? "Review" : "Progress")) : null);
        if (reviewOpen) clear(reviewOpen.body, h("div", { class: "stack", style: { gap: "10px" } }, reviewParts()));
        drawList();
        return;
      }
      clear(reviewBox, card({ cls: "cc-set-revc", title: list.length ? "Your changes" : "Changes", sub: list.length ? "planned by the coordinator" : kube.status?.phase ? `the operator: ${kube.status.phase}` : "", body: h("div", { class: "stack", style: { gap: "10px" } }, reviewParts()) }));
      drawList();
    }
    function openReview() {
      const bodyEl = h("div", { class: "stack", style: { gap: "10px" } }, reviewParts());
      const d = drawer({ title: changedList().length ? "Your changes" : "Changes", sub: `${kube.namespace}/${kube.name}`, body: bodyEl, onClose: () => { reviewOpen = null; } });
      reviewOpen = { body: bodyEl, close: () => d?.close?.() };
    }

    // ---------------------------------------------------------- machines of your own
    /** A setting's flag on a server started by hand. */
    const FLAG = { "spec:writeAcks": "--write-acks", "spec:failoverAfterMs": "--failover-after-ms", "spec:tokenSecret": "--token", "spec:clusterTokenSecret": "--cluster-token" };
    function flagOf(st) {
      if (FLAG[st.at]) return { flag: FLAG[st.at], who: "every shard server" };
      const [where, rest] = st.at.split(/:(.*)/s);
      if (where === "args") return { flag: rest, who: "every shard server" };
      if (where === "cargs") return { flag: rest, who: "the coordinator" };
      return null;
    }
    /** What this page can read of a setting on machines of your own, and from where. */
    function known(st) {
      const L = live(state.cluster), y = state.system;
      const rep = (state.cluster?.shards || []).flatMap((sh) => sh.replicas || []).map((r) => r.detail?.health?.replication).find((x) => x?.role === "primary");
      // What every copy says of a flag: one value, or null when they differ or none says.
      const copies = (state.cluster?.shards || []).flatMap((sh) => sh.replicas || []).filter((r) => r.status !== "down");
      const said = (f) => { const vs = [...new Set(copies.map((r) => f(r)).filter((v) => v != null))]; return vs.length === 1 ? vs[0] : vs.length > 1 ? { mixed: vs } : null; };
      // One value every copy says, or the first with how they differ.
      const each = (f, words) => { const v = said(f); return v == null ? null : v.mixed ? [v.mixed[0], `the copies differ: ${v.mixed.map(words).join(", ")}`] : [v, "every copy says"]; };
      switch (st.at) {
        case "spec:failoverAfterMs": {
          const v = said((r) => r.detail?.capability?.failover_after_ms);
          return v == null ? null : v.mixed ? [v.mixed[0], `the copies differ: ${v.mixed.map((x) => (x ? x + " ms" : "by hand")).join(", ")}`] : [v, "every copy says"];
        }
        case "spec:writeAcks": return L.acks ? [L.acks, "the primaries say"] : null;
        case "args:--verify-every-s": return rep?.verify_every_s != null ? [rep.verify_every_s, "the primaries say"] : null;
        case "cargs:--hedge-ms": return state.cluster?.hedge_ms != null ? [state.cluster.hedge_ms, "this coordinator says"] : null;
        // What each copy runs with (NEW-cluster-admin-5), from its own limits.
        case "args:--slow-ms": return each((r) => r.detail?.limits ? r.detail.limits.slow_ms ?? 0 : null, (x) => (x ? x + " ms" : "off"));
        case "args:--max-concurrent": return each((r) => r.detail?.limits?.max_concurrent, String);
        case "args:--max-result-window": return each((r) => r.detail?.limits ? r.detail.limits.max_rows ?? 0 : null, (x) => (x ? fmt.n(x) : "no limit"));
        case "cargs:--timeout-ms": return y?.limits?.timeout_ms != null ? [y.limits.timeout_ms, "this coordinator says"] : null;
        case "spec:tokenSecret": return y?.auth ? [y.auth.token ? "set" : undefined, "this coordinator says"] : null;
        case "spec:clusterTokenSecret": return y?.auth ? [y.auth.cluster_token ? "set" : undefined, "this coordinator says"] : null;
        default: return null;
      }
    }
    function drawOwn() {
      body.classList.remove("has-bar");
      const x = ctx();
      const rows = SHOWN.map((st) => [st, flagOf(st)]).filter(([, f]) => f);
      const groups = [...new Set(rows.map(([st]) => st.g))];
      const shownOf = (st, v) => (st.type === "text" ? (v ? "set" : "not set") : shown(st, v));
      // chloedb-server's own defaults where the operator's differ.
      const OWNDEF = { "spec:writeAcks": "all", "spec:failoverAfterMs": 0 };
      const TOKENS = {
        "spec:tokenSecret": (v) => (v ? "Clients and admins send this token, or an API key." : "No token: anything that reaches the servers may read, write and change the cluster."),
        "spec:clusterTokenSecret": (v) => (v ? "The servers take only the cluster token from each other: the client token cannot replicate or move slots." : "The client token also makes the calls between servers."),
      };
      const tbl = h("div", { class: "cc-flags" }, groups.map((g) => h("div", { class: "cc-set-g" }, h("div", { class: "cc-set-gt" }, g),
        rows.filter(([st]) => st.g === g).map(([st, f]) => {
          const k = known(st);
          const v = k ? k[0] : (st.at in OWNDEF ? OWNDEF[st.at] : st.def);
          const effect = TOKENS[st.at] ? (k ? TOKENS[st.at](v) : "Each server says it only on its own command line.") : st.effect?.(v, x) || "";
          return h("div", { class: "cc-set" },
            h("div", { class: "cc-set-l" }, h("b", {}, st.label), h("div", { class: "small dim" }, effect)),
            h("div", { class: "cc-set-r" },
              // A value the servers say is bold; a default they do not say is marked as one.
              k ? h("b", {}, shownOf(st, v)) : TOKENS[st.at] ? h("b", {}, "–") : h("span", { class: "muted" }, `default: ${shownOf(st, v)}`),
              h("div", { class: "small muted" }, k ? k[1] : TOKENS[st.at] ? "as each server was started" : "the servers do not say it: this holds unless one was started with the flag"),
              h("code", { class: "cc-set-flag" }, `${f.flag} · ${f.who}`)));
        }))));
      clear(body, h("div", { class: "cc-set-main own" },
        card({ title: "Every server", sub: "machines of your own: each server's flags",
          body: h("div", { class: "stack", style: { gap: "14px" } },
            h("div", { class: "cc-info" }, icon("info"), h("div", {}, "Each server keeps the flags it was started with: there is no spec to edit here. Below is what the cluster says of them, and the flag for each.", kubeErr && !(kubeErr instanceof ApiError && kubeErr.status === 404) ? h("div", { class: "small muted" }, `(The coordinator did not say whether an operator runs it: ${errText(kubeErr)}.)`) : null)),
            tbl) }),
        card({ title: "How to change one", sub: "a server at a time, while the cluster serves",
          body: h("div", { class: "stack", style: { gap: "10px" } },
            h("ol", { class: "cc-set-steps" },
              h("li", {}, "Restart each replica with the new flag, one at a time. Wait until the Cluster canvas shows it in sync again before the next."),
              h("li", {}, "Hand each primary over: on ", h("a", { href: link("cluster") }, "Cluster"), ", open the shard, then a replica's ⋯ menu, Make it the primary. Then restart the old primary with the flag."),
              h("li", {}, "Restart the coordinator last, with its own flags (", h("code", {}, "--hedge-ms"), ", ", h("code", {}, "--timeout-ms"), ", …). While it restarts, the cluster answers no one.")),
            h("div", { class: "small dim" }, "On Kubernetes the operator keeps every setting in the StrataCluster's spec and rolls each change out for you: ", h("a", { href: link("cloud") }, "Cloud setup"), " makes one.")) })));
    }

    return {
      draw() { if (!kube) { clear(body, empty("system", "Asking the coordinator…")); load(); } else draw1(); },
      load, poll() { if (!watching && !applying) load(); },
      stop() { stopWatch?.(); },
      narrowChanged() { if (tab === "settings") draw1(); },
    };
  })();

  // ============================================================ upkeep of the shards (coordinator)
  const fleetUpkeep = (() => {
    let run = null, audit = null, auditErr = null;
    const unsupported = () => store.get("cluster.unsupported", {}) || {};
    async function loadAudit() {
      try { audit = (await api("/v1/admin/audit?limit=1000")).entries || []; auditErr = null; } catch (e) { audit = []; auditErr = e; }
      if (tab === "upkeep") draw();
    }
    function compareCard() {
      const C = comparisons(state.cluster);
      if (!state.cluster) return card({ title: "Replicas compared", body: empty("layers", "Asking the shards…") });
      const every_ = C.every ? (C.every < 120 ? `${C.every} s` : `${Math.round(C.every / 60)} min`) : null;
      const lines = [];
      for (const d of C.differ) lines.push(["bad", `Shard ${d.shard}: ${copyName(d.url)} differed in ${plural(d.n, "slot")}${d.first != null ? ` (the first is slot ${fmt.n(d.first)})` : ""} ${fmt.ago(Date.now() - d.ago)}, and was sent a copy of its primary.`, d.shard]);
      for (const f of C.failed) lines.push(["bad", `Shard ${f.shard}: comparing ${copyName(f.url)} failed: ${f.error}.`, f.shard]);
      for (const i of C.noPrimary) lines.push(["bad", `Shard ${i}: no primary answers, so nothing compares its copies.`, i]);
      for (const n of C.notSynced) lines.push(["warn", `Shard ${n.shard}: ${copyName(n.url)} is not in sync, so it is not compared until it has caught up.`, n.shard]);
      if (C.never.length) lines.push(["", `${plural(C.never.length, "replica")} not compared yet since ${C.never.length === 1 ? "its" : "their"} primary started${C.never.length <= 3 ? ": " + C.never.map((n) => copyName(n.url)).join(", ") : ""}.`, C.never[0].shard]);
      const allGood = !C.differ.length && !C.failed.length && !C.noPrimary.length;
      const summary = C.replicas
        ? `${every_ ? `Each primary compares its replicas' rows with its own every ${every_}. ` : ""}${fmt.n(C.compared)} of ${plural(C.replicas, "replica")} compared${C.oldest != null ? ` (the oldest comparison ${fmt.ago(Date.now() - C.oldest)})` : ""}${allGood && C.compared ? ": every one compared holds its primary's rows." : "."}`
        : C.noPrimary.length ? "No primary answers: there is nothing to compare." : "No primary here ships to a replica: there is nothing to compare.";
      const un = unsupported().verify;
      const pr = C.primaries.filter((p) => p.replicas > 0);
      const btn = un ? null : h("button", { class: "btn sm", disabled: !canAdmin() || !pr.length || run?.running, title: canAdmin() ? "" : needsAdmin, onclick: () => verifyAll(pr) }, icon("refresh"), run?.running ? "Comparing…" : `Compare every shard now (${fmt.n(pr.length)})`);
      const runEl = run ? h("div", { class: "cc-run-v" },
        h("div", { class: "row nw between small" }, h("b", {}, run.running ? `Comparing: ${fmt.n(run.done)} of ${plural(run.total, "shard")}` : run.stopped ? `Stopped after ${fmt.n(run.done)} of ${plural(run.total, "shard")}` : `Compared ${plural(run.done, "shard")} ${fmt.ago(run.end)}`),
          run.running ? h("button", { class: "btn sm ghost", onclick: () => { run.stop = true; } }, "Stop") : null),
        h("div", { class: "cc-meter" }, h("i", { class: "cc-meter-f l0", style: { width: `${(run.done / Math.max(1, run.total)) * 100}%` } })),
        h("div", { class: "small " + (run.bad.length ? "cc-bad" : run.warn.length ? "cc-warn" : "dim") }, run.bad.length ? `${plural(run.bad.length, "shard")} had a replica that differed or failed: ${run.bad.slice(0, 6).map((b) => b.shard).join(", ")}${run.bad.length > 6 ? ", …" : ""}.` : run.warn.length ? `${plural(run.warn.length, "shard")} had a replica not compared (not in sync, or late).` : run.done ? "Every replica compared holds its primary's rows." : ""),
        run.lines.length ? h("ul", { class: "cc-cmp" }, run.lines.slice(0, 8).map((l) => h("li", { class: l.tone === "bad" ? "cc-bad" : l.tone === "warn" ? "cc-warn" : "" }, `Shard ${l.shard}: ${l.text}.`))) : null) : null;
      return card({ title: "Replicas compared", sub: "each primary against its replicas",
        body: h("div", { class: "stack", style: { gap: "8px" } },
          h("div", { class: "small" }, summary),
          btn ? h("div", {}, btn) : null,
          lines.length ? h("ul", { class: "cc-cmp" }, lines.slice(0, 8).map(([t, x, shard]) => h("li", { class: t === "bad" ? "cc-bad" : t === "warn" ? "cc-warn" : "dim" }, x, " ", h("a", { href: link("cluster", { shard }) }, "Open")))) : null,
          lines.length > 8 ? h("div", { class: "small muted" }, `and ${fmt.n(lines.length - 8)} more on the Cluster canvas.`) : null,
          runEl,
          un ? h("div", { class: "small muted" }, "This coordinator cannot ask its primaries to compare on demand (a newer server can): they compare on their own schedule above.") : null) });
    }
    async function verifyAll(pr) {
      const ok = await ask({ title: `Compare every shard's replicas (${fmt.n(pr.length)})`, body: "Each primary asks its replicas for a digest of their rows and compares it with its own; a replica that differs is sent a copy. Queries keep running; a few shards compare at a time.", ok: "Compare them" });
      if (!ok) return;
      const queue = pr.slice();
      run = { running: true, total: queue.length, done: 0, bad: [], warn: [], lines: [], stop: false, stopped: false, end: null };
      draw();
      const worker = async () => {
        while (queue.length && !run.stop) {
          const p = queue.shift();
          try {
            const j = await api("/v1/admin/copies/verify", { body: { url: p.url }, reason: ok.reason, timeout: 70000 });
            const w = verifyWords(j);
            if (w.tone === "bad") run.bad.push(p); else if (w.tone === "warn") run.warn.push(p);
            for (const l of w.lines) if (l.tone !== "ok") run.lines.push({ ...l, shard: p.shard });
          } catch (e) {
            if (e.status === 404 || e.status === 405) { store.set("cluster.unsupported", { ...unsupported(), verify: true }); run.stop = true; queue.length = 0; run = null; draw(); toast("This coordinator cannot ask its primaries to compare on demand: a newer server can.", "bad", 8000); return; }
            run.bad.push(p); run.lines.push({ tone: "bad", text: `not compared: ${errText(e)}`, shard: p.shard });
          }
          run.done++;
          if (tab === "upkeep" && alive) draw();
        }
      };
      await Promise.all([worker(), worker(), worker(), worker()]);
      if (!run) return;
      run.running = false; run.stopped = run.stop && run.done < run.total; run.end = Date.now();
      toast(run.bad.length ? `${plural(run.bad.length, "shard")} had a replica that differed: each is sent a copy` : `Compared ${plural(run.done, "shard")}: the same rows`, run.bad.length ? "bad" : "ok", 8000);
      refreshCluster();
      if (alive) draw();
    }
    function diskCard() {
      const all = (state.cluster?.shards || []).flatMap((sh, i) => (sh.replicas || []).map((r, k) => ({ shard: sh.shard ?? i, url: r.url, down: r.status === "down", disk: r.detail?.minute?.disk_used_ratio ?? r.detail?.load?.disk_used_ratio ?? null, free: r.detail?.load?.disk_free_bytes ?? null, k })));
      const known = all.filter((c) => !c.down && c.disk != null).sort((a, b) => b.disk - a.disk);
      if (!known.length) return card({ title: "Disks", body: h("div", { class: "small dim" }, state.cluster ? "No copy says how full its disk is (an older server)." : "Asking the shards…") });
      const red = known.filter((c) => lvl(c.disk, DISK) === 2), amber = known.filter((c) => lvl(c.disk, DISK) === 1);
      const fd = state.fleet?.checks?.find?.((x) => x.id === "disk");
      const say = red.length ? `${plural(red.length, "copy", "copies")} of ${fmt.n(known.length)} over 90% full: a merge needs room to write its new segment beside the old ones, and may fail there.` : amber.length ? `${plural(amber.length, "copy", "copies")} of ${fmt.n(known.length)} over 80% full: plan more room soon.` : `Every copy's disk is under 80% full; the fullest is ${fmt.pct(known[0].disk, 0)}.`;
      return card({ title: "Disks", sub: "each copy's, amber from 80%, red from 90%",
        tools: h("a", { class: "btn sm", href: link("cluster", { lens: "disk" }) }, "See every disk", icon("right")),
        body: h("div", { class: "stack", style: { gap: "8px" } },
          h("div", { class: "small " + (red.length ? "cc-bad" : amber.length ? "cc-warn" : "") }, say),
          fd?.days_to_full != null ? h("div", { class: "small dim" }, `At the last week's growth the fullest is full in ${fullIn(fd.days_to_full)}.`) : null,
          h("div", { class: "cc-disks" }, known.slice(0, 5).map((c) => h("a", { class: "cc-disk", href: link("cluster", { shard: c.shard, lens: "disk" }) }, meter(c.disk, { ...DISK, why: {} }, `${copyName(c.url)} · shard ${c.shard}${c.free != null ? ` · ${fmt.bytes(c.free)} free` : ""}`)))),
          known.length > 5 ? h("div", { class: "small muted" }, `The ${fmt.n(Math.min(5, known.length))} fullest of ${fmt.n(known.length)}.`) : null) });
    }
    function copyOpsCard() {
      const cps = (audit || []).filter((e) => /checkpoint/.test(e.route || "") && (e.method || "POST") !== "GET");
      const lastCp = cps[0];
      return card({ title: "A copy's operations", sub: "on the Cluster canvas",
        tools: h("a", { class: "btn sm", href: link("cluster") }, "Open Cluster", icon("right")),
        body: h("div", { class: "stack", style: { gap: "8px" } },
          h("div", { class: "small" }, "Open a shard on the canvas, then a copy's ⋯ menu: compare its replicas, write a checkpoint into its server's --backup-dir, make it the primary, or remove it. Each says there why it is not offered when it is not."),
          h("div", { class: "small dim" }, lastCp ? [`Last checkpoint asked through this coordinator: ${fmt.ago(lastCp.ts_ms)}${lastCp.who ? ` by ${lastCp.who}` : ""}`, lastCp.detail ? ` (${lastCp.detail})` : "", lastCp.status && lastCp.status >= 400 ? `, which failed (${lastCp.status})` : "", "."]
            : audit && !auditErr ? "No checkpoint asked through this coordinator in its audit log." : auditErr instanceof ApiError && auditErr.status === 403 ? "Its audit log, which says the last checkpoint asked, needs an admin key." : "")) });
    }
    function statsCard() {
      const res = results.get("coord:reset");
      return card({ title: "Statement statistics", sub: "this coordinator's",
        body: h("div", { class: "stack", style: { gap: "8px" } },
          h("div", { class: "small dim" }, "The per-statement counts of Queries, since this coordinator started or since they were last reset."),
          h("div", { class: "row", style: { gap: "8px" } }, h("button", { class: "btn sm", disabled: !canAdmin(), title: canAdmin() ? "" : needsAdmin, onclick: resetStats }, "Reset them…"),
            res ? h("span", { class: "small " + (res.ok ? "dim" : "cc-bad") }, `${res.text} (${fmt.ago(res.at)})`) : null)) });
    }
    async function resetStats() {
      const ok = await ask({ title: "Reset the statement statistics?", body: "Every per-statement count of Queries starts again from zero. The query log keeps what ran.", ok: "Reset them", danger: true });
      if (!ok) return;
      try { await api("/v1/queries/stats/reset", { body: {}, reason: ok.reason }); results.set("coord:reset", { ok: true, text: "Reset: the counts start again from zero", at: Date.now() }); toast("The statement statistics start again from zero", "ok"); }
      catch (e) { results.set("coord:reset", { ok: false, text: "Not reset: " + errText(e), at: Date.now() }); }
      draw();
    }
    return {
      draw() {
        if (audit == null) { audit = []; loadAudit(); }
        clear(body, h("div", { class: "cc-up" }, h("div", { class: "cc-up-c" }, compareCard(), copyOpsCard()), h("div", { class: "cc-up-c" }, diskCard(), statsCard())));
      },
    };
  })();

  // ============================================================ upkeep of this server (not a cluster)
  const serverUpkeep = (() => {
    const segs = h("div"), merges = h("div");
    let audit = null, running = new Set();
    const id = () => `${state.system?.pid ?? "?"}:${state.system?.started_ms ?? ""}`;
    async function loadAudit() { try { audit = (await api("/v1/admin/audit?limit=1000")).entries || []; } catch { audit = []; } if (tab === "upkeep") drawOps(); }
    let opsBox = null, dbBox = null, rowsAll = null, rowsAt = 0;
    // The rows of every table (the metrics' docs count only the default one).
    const countRows = () => { if (Date.now() - rowsAt < 60000) return; rowsAt = Date.now(); api("/v1/index/tables", { timeout: 4000 }).then((j) => { rowsAll = (j.tables || []).reduce((a, x) => a + (x.docs || 0), 0); drawDb(); }).catch(() => {}); };
    function drawDb() {
      const y = state.system;
      if (!dbBox) return;
      if (!y) { clear(dbBox, card({ title: "Database", body: empty("system", "Loading…") })); return; }
      const p = last() || {};
      if (!y.db) { clear(dbBox, card({ title: "Database", body: h("div", { class: "small dim" }, "This server holds no database.") })); return; }
      const d = y.db.disk;
      const used = d ? 1 - d.available_bytes / d.total_bytes : null;
      clear(dbBox, card({ title: "Database", sub: y.db.path,
        body: h("div", { class: "stack", style: { gap: "10px" } },
          d ? meter(used, DISK, `disk: ${fmt.bytes(d.available_bytes)} free of ${fmt.bytes(d.total_bytes)}`) : null,
          trend("disk_used_ratio", { ratio: true }),
          kv([["tables", y.db.tables], ["sequence", y.db.seq_no ? `term ${y.db.seq_no.term} · write ${fmt.n(y.db.seq_no.seq)}` : "not numbered (--seq-no)"], ["rows", (countRows(), fmt.n(rowsAll ?? p.docs))], ["file", fmt.bytes(p.file_bytes)], ["log", fmt.bytes(p.wal_bytes)]]),
          trend("file_bytes")) }));
    }
    const OPS = () => {
      const y = state.system || {};
      const local = y.backend === "local";
      const repl = state.health?.replication;
      const primary = repl?.role === "primary";
      const dir = y.features?.backup_dir;
      const lastCp = (audit || []).find((e) => e.route === "/v1/admin/checkpoint" && e.method !== "GET");
      const cmp = primary ? (repl.replicas || []).map((f) => f.verified?.ms_ago).filter((x) => x != null) : [];
      return [
        { id: "check", ic: "okc", title: "Check the database", sub: "Reads every page and checks every checksum and structure; writes wait meanwhile. A while on a large file.", no: !local && "On a server with a database.", warn: "Reads the whole file; writes wait until it ends. Queries keep running.", path: "/v1/admin/check" },
        { id: "vacuum", ic: "disk", title: "Vacuum", sub: "Gives the free space at the end of the file back to the file system: moves segments down and truncates the tail.", no: !local && "On a server with a database.", warn: "Rewrites the end of the file; writes wait meanwhile.", danger: true, path: "/v1/admin/vacuum" },
        { id: "checkpoint", ic: "download", title: "Write a checkpoint", sub: dir ? ["A consistent copy of the database into ", h("code", {}, dir), "."] : "A consistent copy of the database into --backup-dir.",
          note: lastCp ? `Last written ${fmt.ago(lastCp.ts_ms)}${lastCp.who ? ` by ${lastCp.who}` : ""}${lastCp.status >= 400 ? `, which failed (${lastCp.status})` : ""}.` : dir && audit ? "None written since this server started (its audit log keeps the latest)." : null,
          no: !local ? "On a server with a database." : !dir ? "Start the server with --backup-dir DIR to write checkpoints there." : null, run: checkpoint },
        { id: "verify", ic: "topology", title: "Compare the replicas", sub: `Compares each replica's rows with this primary's; a replica that differs is sent a copy.${repl?.verify_every_s ? ` It also runs on its own every ${repl.verify_every_s < 120 ? repl.verify_every_s + " s" : Math.round(repl.verify_every_s / 60) + " min"}.` : ""}`,
          note: cmp.length ? `Last compared ${fmt.ago(Date.now() - Math.min(...cmp))}.` : null,
          no: !primary && (repl?.role === "replica" ? "This server is a replica: its primary compares it." : "On a primary with replicas (--replicas)."), path: "/v1/admin/verify", body: { timeout_ms: 60000 } },
        { id: "snapshot", ic: "clock", title: "Take a snapshot lease", sub: "Pins the rows as they are now, so that several queries read the same rows: pass its id as \"snapshot\".", no: !local && "On a server with a database.", path: "/v1/snapshot" },
        { id: "reset", ic: "refresh", title: "Reset statement statistics", sub: "Starts the per-statement counts of Queries again from zero.", warn: "Every count starts again from zero.", danger: true, path: "/v1/queries/stats/reset" },
      ];
    };
    /** An operation's answer, in words. */
    function said(op, j, ms) {
      const t = ` (${fmt.ms(ms)})`;
      if (op === "check") return { ok: true, text: "Every page, checksum and structure is sound" + t };
      if (op === "vacuum") return { ok: true, text: (j?.bytes_freed ? `Gave ${fmt.bytes(j.bytes_freed)} back to the file system` : "Nothing to give back: the end of the file is in use") + t };
      if (op === "checkpoint") { const c = j?.checkpoint || {}; return { ok: true, text: `Wrote ${c.name || "the checkpoint"}: ${plural(c.tables ?? 0, "table")}, ${plural(c.segments ?? 0, "segment")}${c.segments_linked ? ` (${fmt.n(c.segments_linked)} linked from an earlier one)` : ""}, ${fmt.n(c.live_docs ?? 0)} rows, ${fmt.bytes(c.bytes_written ?? 0)} written, at commit ${fmt.n(c.txn ?? 0)}` + t }; }
      if (op === "verify") { const w = verifyWords(j); return { ok: w.tone !== "bad", tone: w.tone, text: w.summary + t, lines: w.lines.map((l) => l.text) }; }
      if (op === "snapshot") return { ok: true, text: `Snapshot ${j?.snapshot} pins commit ${fmt.n(j?.txn)} for ${j?.ttl_s ?? "–"} s: pass "snapshot": "${j?.snapshot}" with each query`, copy: j?.snapshot };
      if (op === "reset") return { ok: true, text: "The counts start again from zero" };
      return { ok: true, text: "Done" + t };
    }
    async function runOp(op, path, b, warn, danger, extra) {
      if (warn) { const ok = await ask({ title: `${op.title}?`, body: warn, ok: op.title, danger }); if (!ok) return; extra = { ...extra, reason: ok.reason }; }
      running.add(op.id); drawOps();
      const t0 = performance.now();
      try {
        const j = await api(path, { body: b || {}, reason: extra?.reason, timeout: 120000 });
        const r = { ...said(op.id, j, performance.now() - t0, extra), at: Date.now(), raw: j };
        if (op.id === "checkpoint" && extra?.name && r.text) r.text = r.text.replace("the checkpoint", extra.name);
        results.set(`${id()}:${op.id}`, r);
        toast(`${op.title}: ${r.text}`, r.ok ? "ok" : "bad", 6000);
        if (op.id === "checkpoint") loadAudit();
        loadStats(); refreshSystem();
      } catch (e) { results.set(`${id()}:${op.id}`, { ok: false, text: "Failed: " + errText(e), at: Date.now() }); }
      running.delete(op.id);
      drawOps();
    }
    function checkpoint(op) {
      const name = "cp-" + new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-");
      const inp = h("input", { class: "input", value: name });
      const layout = h("select", { class: "select" }, h("option", { value: "file" }, "one file"), h("option", { value: "dir" }, "a directory"));
      const go = h("button", { class: "btn primary" }, "Write it");
      const m = modal({ title: "Write a checkpoint", body: [h("label", { class: "f" }, `Name, in ${state.system.features.backup_dir}`, inp), h("label", { class: "f" }, "Layout", layout)], foot: [h("button", { class: "btn", onclick: () => m.close() }, "Cancel"), go] });
      go.onclick = () => { m.close(); runOp(op, "/v1/admin/checkpoint", { name: inp.value.trim(), layout: layout.value }, null, false, { name: inp.value.trim() }); };
    }
    function drawOps() {
      if (!opsBox) return;
      const rows = OPS().map((op) => {
        const res = results.get(`${id()}:${op.id}`);
        const busy = running.has(op.id);
        // Bounded operations (a checkpoint, a verify, a reset) an ops key may run too: the catalog says.
        const route = `POST ${op.path || (op.id === "checkpoint" ? "/v1/admin/checkpoint" : "")}`;
        const no = op.no || (op.id !== "snapshot" && !can("admin", route) ? needs("admin", route) : null);
        return h("div", { class: "cc-op" + (no ? " off" : "") },
          h("div", { class: "cc-op-ic" }, icon(op.ic)),
          h("div", { class: "cc-op-t" },
            h("b", {}, op.title), h("div", { class: "small dim" }, op.sub),
            op.note ? h("div", { class: "small muted" }, op.note) : null,
            no ? h("div", { class: "small muted" }, no) : null,
            res ? h("div", { class: "cc-op-res small " + (res.tone === "warn" ? "cc-warn" : res.ok ? "" : "cc-bad") }, icon(res.ok ? "okc" : "alert"), h("span", {}, res.text, ` · ${fmt.ago(res.at)}`),
              res.copy ? h("button", { class: "btn sm ghost", onclick: () => copy(res.copy, "Snapshot id copied") }, icon("copy")) : null) : null,
            res?.lines?.length > 1 || (res?.lines?.length === 1 && res.tone !== "ok") ? h("ul", { class: "cc-cmp" }, res.lines.map((l) => h("li", { class: "small" }, cap(l) + "."))) : null,
            res?.raw ? h("details", { class: "small" }, h("summary", { class: "muted" }, "The answer"), h("pre", { class: "code" }, JSON.stringify(res.raw, null, 2))) : null),
          h("button", { class: "btn sm" + (op.danger ? " danger" : ""), disabled: !!no || busy, onclick: () => (op.run ? op.run(op) : runOp(op, op.path, op.body, op.warn, op.danger)) }, busy ? "Running…" : "Run"));
      });
      clear(opsBox, card({ title: "Operations", sub: "on this server", body: h("div", { class: "cc-ops" }, rows) }));
    }
    async function loadStats() {
      try {
        const j = await api("/v1/stats");
        const st = j.storage, mg = j.merges;
        if (!st) { clear(segs); clear(merges); return; }
        const byTable = {};
        for (const sg of st.segments) (byTable[sg.table] ||= []).push(sg);
        const maxDocs = Math.max(1, ...st.segments.map((x) => x.n_docs));
        const tables = Object.entries(byTable);
        clear(segs, card({ title: "Segments", sub: "each box a segment, its width its rows; red is deleted rows", body: [tables.length ? tables.map(([t, list], ti) => {
          const total = list.reduce((a, x) => a + x.n_docs, 0);
          const W = 1000, H = 34;
          const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, style: { width: "100%", height: H + "px" }, preserveAspectRatio: "none" });
          let x = 0;
          for (const sg of list.slice().sort((a, b) => b.n_docs - a.n_docs)) {
            const w = Math.max(3, sg.n_docs / Math.max(1, total) * W);
            const dead = sg.n_deleted / Math.max(1, sg.n_docs);
            const g = s("g");
            g.append(s("rect", { x: x + 1, y: 2, width: Math.max(1, w - 2), height: H - 4, rx: 4, style: { fill: SERIES[ti % 8], opacity: 0.35 + 0.65 * Math.sqrt(sg.n_docs / maxDocs) } }));
            if (dead > 0) g.append(s("rect", { x: x + 1, y: H - 8, width: Math.max(1, (w - 2) * dead), height: 6, rx: 2, style: { fill: "var(--crit)" } }));
            g.addEventListener("pointermove", (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `${t} · segment ${sg.id}`), h("div", { class: "r" }, h("b", {}, fmt.n(sg.n_docs) + " rows"), h("span", { class: "n" }, fmt.bytes(sg.bytes))), h("div", { class: "r" }, h("b", {}, fmt.n(sg.n_deleted)), h("span", { class: "n" }, "deleted, " + fmt.pct(dead)))]));
            g.addEventListener("pointerleave", hideTip);
            svgEl.append(g);
            x += w;
          }
          return h("div", { style: { marginBottom: "12px" } }, h("div", { class: "row nw between small", style: { marginBottom: "4px", gap: "8px" } }, h("b", {}, t), h("span", { class: "muted" }, `${plural(list.length, "segment")} · ${fmt.n(total)} rows · ${fmt.bytes(list.reduce((a, x2) => a + x2.bytes, 0))}`)), svgEl);
        }) : empty("layers", "No segments yet", "Rows wait in the memtable until it flushes."),
        h("div", { class: "row small muted", style: { gap: "6px 16px" } }, h("span", {}, `memtable: ${fmt.n(st.memtable_docs)} rows`), h("span", {}, `pending deletes: ${fmt.n(st.pending_deletes)}`), h("span", {}, `pages: ${fmt.n(st.live_pages)} live · ${fmt.n(st.free_pages)} free · ${fmt.n(st.pending_pages)} held`))] }));
        clear(merges, card({ title: "Merges", body: mg ? kv([["running", `${mg.running} (${plural(mg.segments_merging ?? 0, "segment")})`], ["memory", `${fmt.bytes(mg.memory)}${mg.memory_budget ? " of " + fmt.bytes(mg.memory_budget) : ""}`], ["done", fmt.n(mg.merges)], ["made smaller", fmt.n(mg.merges_limited)], ...Object.entries(mg).filter(([k]) => !["running", "segments_merging", "memory", "memory_budget", "merges", "merges_limited"].includes(k)).map(([k, v]) => [k.replace(/_/g, " "), v == null ? "none" : typeof v === "object" ? JSON.stringify(v) : typeof v === "number" ? fmt.n(v) : String(v)])]) : h("div", { class: "small muted" }, "–") }));
      } catch (e) { clear(segs, card({ title: "Segments", body: failure(e, "Storage") })); }
    }
    return {
      draw() {
        opsBox = h("div"); dbBox = h("div");
        clear(body, h("div", { class: "cc-up" }, h("div", { class: "cc-up-c" }, dbBox, merges), h("div", { class: "cc-up-c wide" }, opsBox, segs)));
        drawDb(); drawOps(); loadStats(); if (audit == null) loadAudit();
      },
      refresh() { if (tab === "upkeep" && !coord()) { drawDb(); drawOps(); } },
      poll() { if (tab === "upkeep" && !coord()) loadStats(); },
    };
  })();

  // ============================================================ this server: process, limits, config, metrics
  const serverTab = (() => {
    let procBox = null, limBox = null, cfg = null, prom = null, cfgJ = null, promText = null, promQ = "";
    function drawProc() {
      const y = state.system;
      if (!procBox) return;
      if (!y) { clear(procBox, card({ title: "Process", body: empty("system", "Loading…") })); clear(limBox); return; }
      const memRatio = y.memory_limit_bytes ? y.rss_bytes / y.memory_limit_bytes : null;
      clear(procBox, card({ title: "Process", sub: `${y.backend === "coordinator" ? "the coordinator" : y.role}${y.version ? " · " + y.version : ""}`,
        body: h("div", { class: "stack", style: { gap: "10px" } },
          memRatio != null ? meter(memRatio, MEM, `memory: ${fmt.bytes(y.rss_bytes)} of ${fmt.bytes(y.memory_limit_bytes)}`) : h("div", { class: "small" }, `Memory: ${fmt.bytes(y.rss_bytes)} resident, no limit known.`),
          trend("rss_bytes"),
          kv([["version", y.version], ["backend", `${y.backend} · ${y.role}`], ["up", `${fmt.dur(y.uptime_s)} (since ${fmt.datetime(y.started_ms)})`], ["pid", y.pid], ["platform", `${y.os} ${y.arch}`], ["SIMD", y.simd], ["cores", y.cores], ["CPU time", fmt.dur(y.cpu_seconds)], ["log filter", h("span", { class: "mono" }, y.logs?.filter)], ["sampling", `a point every ${fmt.ms(y.sampling?.interval_ms)}, ${fmt.n(y.sampling?.points)} kept`]])) }));
      const L = y.limits || {}, F = y.features || {};
      const p = last() || {};
      const qm = L.query_memory_bytes && p.query_memory_bytes != null ? p.query_memory_bytes / L.query_memory_bytes : null;
      clear(limBox, card({ title: "Limits", sub: "as this server was started",
        body: h("div", { class: "stack", style: { gap: "10px" } },
          qm != null ? meter(qm, MEM, `query memory in use: ${fmt.bytes(p.query_memory_bytes)} of ${fmt.bytes(L.query_memory_bytes)}`) : null,
          kv([["timeout", `${fmt.ms(L.timeout_ms)} (at most ${fmt.ms(L.max_timeout_ms)})`], ["queries at once", L.max_concurrent], ["rows per answer", fmt.n(L.max_rows)], ["groups", fmt.n(L.max_groups)], ["query memory", L.query_memory_bytes ? fmt.bytes(L.query_memory_bytes) : "no budget"], ["queued writes", fmt.n(L.max_queued_writes)], ["body", fmt.bytes(L.max_body_bytes)], ["slow queries", L.slow_ms ? `over ${L.slow_ms} ms` : "not kept"], ["leases", `${L.lease_ttl_s} s`], ["synonym rules", F.synonyms], ["models", F.models], ["backups", F.backup_dir ? h("span", { class: "mono small" }, F.backup_dir) : "no --backup-dir"], ["agent key", F.agent_key ? "set" : "not set"], ["token", y.auth?.token ? "set" : "none"]])) }));
    }
    function drawCfg() {
      if (!cfg) return;
      clear(cfg, card({ title: "Configuration", sub: "GET /v1/config", body: cfgJ instanceof Error ? failure(cfgJ, "The configuration") : cfgJ ? h("pre", { class: "code", style: { maxHeight: "360px" } }, JSON.stringify(cfgJ, null, 2)) : empty("system", "Loading…") }));
    }
    function drawProm() {
      if (!prom) return;
      if (promText instanceof Error) { clear(prom, card({ title: "Prometheus metrics", sub: "GET /metrics", body: failure(promText, "The metrics") })); return; }
      if (promText == null) { clear(prom, card({ title: "Prometheus metrics", sub: "GET /metrics", body: empty("chart", "Loading…") })); return; }
      const q = h("input", { class: "input sm", type: "search", placeholder: "Filter metrics…", value: promQ, "aria-label": "Filter metrics", style: { flex: "1", minWidth: "0" } });
      const pre = h("pre", { class: "code", style: { maxHeight: "320px" } });
      const draw2 = () => { const f = promQ.toLowerCase(); pre.textContent = promText.split("\n").filter((l) => !f || l.toLowerCase().includes(f)).join("\n"); };
      q.oninput = debounce(() => { promQ = q.value; draw2(); }, 120);
      draw2();
      clear(prom, card({ title: "Prometheus metrics", sub: "GET /metrics", body: [h("div", { class: "row nw", style: { gap: "6px", marginBottom: "8px" } }, q, h("button", { class: "btn sm icon", "aria-label": "Copy the metrics", onclick: () => copy(promText, "Metrics copied") }, icon("copy"))), pre, h("div", { class: "small muted" }, `${fmt.n(promText.split("\n").filter((l) => l && !l.startsWith("#")).length)} samples · scrape them at ${location.origin}/metrics`)] }));
    }
    async function loadCfg() { try { cfgJ = await api("/v1/config"); } catch (e) { cfgJ = e; } drawCfg(); }
    async function loadProm() { try { promText = await api("/metrics", { text: true }); } catch (e) { promText = e; } drawProm(); }
    return {
      draw() {
        procBox = h("div"); limBox = h("div"); cfg = h("div"); prom = h("div");
        clear(body, h("div", { class: "grid g2 cc-srv" }, procBox, limBox, cfg, prom));
        drawProc(); drawCfg(); drawProm();
        if (cfgJ == null) loadCfg();
        if (promText == null) loadProm();
      },
      refresh() { if (tab === "server") drawProc(); },
    };
  })();

  // ------------------------------------------------------------ lifecycle
  draw();
  refreshSystem();
  if (coord()) refreshCluster();
  // Opened before the server said what it is: the tabs follow once it does.
  let wasCoord = coord();
  const recheck = () => { if (coord() === wasCoord) return false; wasCoord = coord(); tab = pick(new URLSearchParams(location.hash.split("?")[1] || "")); if (coord()) refreshCluster(); draw(); return true; };
  let wasNarrow = narrow();
  const onResize = debounce(() => { const n = narrow(); if (n !== wasNarrow) { wasNarrow = n; fleet.narrowChanged(); } }, 150);
  addEventListener("resize", onResize);
  // A redraw that would take the focus away from someone typing waits.
  const typing = () => { const a = document.activeElement; return !!a && body.contains(a) && /^(INPUT|TEXTAREA|SELECT)$/.test(a.tagName); };
  const offs = [
    bus.on("system", () => { if (recheck()) return; if (!typing()) { serverTab.refresh(); serverUpkeep.refresh(); } }),
    bus.on("health", () => { if (recheck()) return; if (!typing()) serverUpkeep.refresh(); }),
    bus.on("cluster", () => { if (recheck() || typing()) return; if (tab === "upkeep" && coord()) fleetUpkeep.draw(); }),
    bus.on("narrow", () => fleet.narrowChanged()),
    every(5000, () => serverUpkeep.poll(), false),
    every(10000, () => { if (coord()) fleet.poll(); }, false),
  ];
  active = {
    onParams(r2) { const t = pick(r2?.params); if (t !== tab) { tab = t; draw(); } },
  };
  return () => { alive = false; active = null; fleet.stop(); removeEventListener("resize", onResize); offs.forEach((f) => f && f()); hideTip(); };
}
