// Home: is the fleet well, and if not what, where, since when, and what to
// do next. A banner from the fleet's health (never green while a headline
// card is red), the headline numbers each naming the worst copy, a Where
// strip with a cell per shard (or per copy, by node or zone), the problems
// ranked with their next step, error ratio and p99 with the changes of the
// window, capacity, the coordinators side by side, and who is affected.

import { can, h, icon, clear, fmt, card, every, api, empty, bus, seg, store, tipOn, drawer, link, when, whenTitle, span, copyName, copyTitle, copy, timeEl, isPhone, logsLink, limited, cannotSee } from "../core.js";
import { tables as catalogTables } from "../catalog.js";
import { isMine } from "../owners.js";
import { timeChart, sparkline, SERIES } from "../charts.js";
import { state, visible, domain, setZoom, isCoordinator, timeRange, fresh, staleText, firingNow, clusterName, environment, rangeParams, rangeSince, isPast, shardState } from "../data.js";

export default { id: "home", title: "Home", icon: "overview", group: "Operate", keywords: "overview home fleet health status incident banner problems slo where capacity coordinators affected dashboard morning check", render, badge };

/** Problems on Home, for the navigation. */
function badge() {
  const f = state.fleet; if (!f) return null;
  const n = (f.problems || []).length;
  return n ? { n, tone: f.status === "critical" ? "crit" : "warn", title: `${fmt.plural(n, "problem")}, worst first on Home` } : null;
}

const SLO_ERR = 0.01, SLO_P99 = 250;
const check = (id) => (state.fleet?.checks || []).find((c) => c.id === id) || null;
const sloErr = () => check("error_ratio")?.slo ?? SLO_ERR;
const sloP99 = () => check("p99")?.slo_ms ?? SLO_P99;
/** One p99 wherever Home names it (F4): the last minute's when the server says it, else its window's. */
const p99Shown = () => { const p = check("p99"); return p?.recent?.value_ms ?? p?.value_ms ?? null; };
const median = (xs) => { const v = xs.filter((x) => x != null && Number.isFinite(x)).sort((a, b) => a - b); return v.length ? v[Math.floor(v.length / 2)] : null; };
const pctTxt = (v) => v == null ? "–" : v < 0.001 && v > 0 ? "<0.1%" : fmt.pct(v, v < 0.1 ? 2 : 1);
const durTxt = (ms) => ms == null ? "" : fmt.dur(Math.max(0, (Date.now() - ms) / 1000)).replace(/ 0s$/, "");
/** A span in words: "45 s", "6 min", "2 h 5 min". */
const durOf = (ms) => { const s = Math.round(ms / 1000); if (s < 90) return `${s} s`; const m = Math.round(s / 60); return m < 90 ? `${m} min` : `${Math.floor(m / 60)} h${m % 60 ? ` ${m % 60} min` : ""}`; };

// ---------------------------------------------------------------- the fleet, copy by copy
/** Every shard with its copies' numbers, from /v1/cluster. */
function model() {
  const c = state.cluster;
  const shards = [];
  const nodes = new Map(), zones = new Map();
  let total = 0, up = 0;
  for (const [i, sh] of (c?.shards || []).entries()) {
    const n = sh.shard ?? i;
    const reps = sh.replicas || [];
    const prim = reps.find((r) => r.role === "primary" && r.status === "ok") || reps.find((r) => r.url === sh.primary) || reps.find((r) => r.role === "primary");
    const behind = {};
    for (const r of reps) for (const x of r.detail?.health?.replication?.replicas || []) if (x.url) behind[x.url] = Math.max(behind[x.url] || 0, x.behind || 0);
    const copies = reps.map((r, j) => {
      const L = r.detail?.load || {};
      const sys = r.detail?.system || {};
      const ok = r.status === "ok" && r.role !== "unknown";
      const name = copyName(r.url, c);
      const x = {
        url: r.url, name, j, shard: n, up: ok, role: ok ? r.role : "down", was: r.last_role || r.role, primary: r === prim,
        node: r.node || null, zone: r.zone || null, host: hostOf(r.url), version: r.version?.version || null,
        p99: L.p99_ms ?? null, qps: L.qps ?? null, errs: L.errors_s ?? null, cpu: L.cpu != null && sys.cores ? L.cpu / sys.cores : null,
        mem: L.rss_bytes != null && sys.memory_limit_bytes ? L.rss_bytes / sys.memory_limit_bytes : L.query_memory_ratio ?? null,
        disk: L.disk_used_ratio ?? null, slots: L.slots_free != null && (sys.limits?.max_concurrent || state.system?.limits?.max_concurrent) ? Math.max(0, 1 - L.slots_free / (sys.limits?.max_concurrent || state.system.limits.max_concurrent)) : null,
        lag: behind[r.url] ?? null, alerts: r.detail?.alerts?.firing || [], down_since: r.down_since_ms || null, avoided: !!r.avoided, why: r.avoided_why || null,
      };
      x.err = x.qps ? (x.errs || 0) / x.qps : null;
      total++; if (ok) up++;
      const nk = x.node || null;
      if (nk) { if (!nodes.has(nk)) nodes.set(nk, []); nodes.get(nk).push(x); }
      if (x.zone) { if (!zones.has(x.zone)) zones.set(x.zone, []); zones.get(x.zone).push(x); }
      return x;
    });
    const st = shardState(sh);
    shards.push({ n, sh, st, why: sh.why || null, copies, primary: prim?.url || null });
  }
  return { shards, nodes, zones, total, up, at: c?.at_ms || null };
}
const hostOf = (u) => { try { return new URL(u).host; } catch { return String(u || ""); } };

// The server's runbooks, by title (a fleet problem's next.runbook names one).
const RUNBOOKS = "https://chloetech-ai.github.io/chloedb/site/docs/runbooks.html#";
/** A copy's tone: crit (down, or hurting queries now), warn (needs a look), or "". */
function copyTone(x) {
  if (!x.up) return "crit";
  if ((x.err != null && x.err > sloErr() && x.qps > 1) || (x.p99 != null && x.p99 > sloP99())) return "crit";
  if ((x.lag != null && x.lag > 1000) || (x.disk != null && x.disk > 0.85) || x.alerts.length || x.avoided) return "warn";
  return "";
}
/** A shard's cell under a lens: { tone, tag, value }. */
function shardCell(s, lens) {
  const worst = (f) => { let v = null; for (const x of s.copies) { const y = f(x); if (y != null && (v == null || y > v)) v = y; } return v; };
  if (lens === "p99") { const v = worst((x) => x.p99); return { tone: v == null ? "" : v > sloP99() ? "crit" : v > sloP99() * 0.6 ? "warn" : "", tag: v != null && v > sloP99() * 0.6 ? fmt.ms(v).replace(" ", "") : null, value: v != null ? `worst p99 ${fmt.ms(v)}` : "p99 not known" }; }
  if (lens === "errors") { const v = worst((x) => x.err); return { tone: v == null ? "" : v > sloErr() ? "crit" : v > sloErr() / 4 ? "warn" : "", tag: v != null && v > sloErr() / 4 ? pctTxt(v) : null, value: v != null ? `${pctTxt(v)} of its queries fail` : "errors not known" }; }
  if (lens === "disk") { const v = worst((x) => x.disk); return { tone: v == null ? "" : v > 0.9 ? "crit" : v > 0.8 ? "warn" : "", tag: v != null && v > 0.8 ? fmt.pct(v, 0) : null, value: v != null ? `fullest disk ${fmt.pct(v, 0)}` : "disk not known" }; }
  if (lens === "lag") { const v = worst((x) => x.lag); return { tone: v == null ? "" : v > 10000 ? "crit" : v > 1000 ? "warn" : "", tag: v != null && v > 1000 ? fmt.si(v) : null, value: v != null ? `a copy ${fmt.si(v)} transactions behind` : "lag not known" }; }
  // Health: what serves, then the worst copy.
  if (["no_reads", "no_writes", "down"].includes(s.st)) return { tone: "crit", tag: s.st === "down" ? "dark" : s.st === "no_reads" ? "no reads" : "no writes", value: s.why || stText(s.st) };
  const tones = s.copies.map(copyTone);
  const down = s.copies.filter((x) => !x.up).length;
  if (tones.includes("crit") && !down) return { tone: "crit", tag: "hot", value: "a copy hurts queries now" };
  if (down) return { tone: "warn", tag: "down", value: `${down} of ${s.copies.length} copies do not answer; the others serve` };
  if (tones.includes("warn")) { const x = s.copies.find((c) => copyTone(c) === "warn"); return { tone: "warn", tag: x.lag > 1000 ? "lag" : x.disk > 0.85 ? "disk" : x.alerts.length ? "alert" : "look", value: x.lag > 1000 ? `${x.name} is ${fmt.si(x.lag)} transactions behind` : x.disk > 0.85 ? `${x.name}'s disk is ${fmt.pct(x.disk, 0)} full` : x.alerts.length ? `${x.name}: ${x.alerts[0].name}` : `${x.name} is avoided after failures` }; }
  return { tone: "", tag: null, value: "fine" };
}
const stText = (st) => ({ ok: "serves reads and writes", degraded: "a copy is down; it still serves", no_reads: "serves no reads", no_writes: "takes no writes", down: "does not answer" })[st] || st;

// ---------------------------------------------------------------- the view
function render(page, r0) {
  const head = h("div", { class: "page-head" });
  const banner = h("div", { class: "banner home-banner" });
  const kpiRow = h("div", { class: "grid g6 home-kpis" });
  // A server with none of the user's tables: where to start (T1).
  const startNudge = h("div");
  page.append(head, startNudge, banner, kpiRow);

  // Where, and the problems.
  // The lens and grouping come from the link first, so a shared link shows what its sender saw (SH-3).
  const LENSES = ["health", "p99", "errors", "disk", "lag"], GROUPS = ["shard", "node", "zone"];
  const fromUrl = (k, ok) => { const v = r0?.params?.get(k); return ok.includes(v) ? v : null; };
  let lens = fromUrl("lens", LENSES) || store.get("home.lens", "health"), group = fromUrl("group", GROUPS) || store.get("home.group", "shard");
  const writeHash = () => {
    const raw = location.hash.replace(/^#\/?/, ""), [path, q] = raw.split("?");
    if (path && path !== "home") return;
    const ps = new URLSearchParams(q || "");
    if (lens !== "health") ps.set("lens", lens); else ps.delete("lens");
    if (group !== "shard") ps.set("group", group); else ps.delete("group");
    const next = "#/home" + (ps.toString() ? "?" + ps : "");
    if (next !== location.hash) { try { history.replaceState(null, "", location.pathname + location.search + next); } catch { /* old browser */ } }
  };
  writeHash();
  const whereBody = h("div", { class: "home-where" });
  const whereSub = h("span", { class: "sub" });
  const lensSeg = seg([["health", "Health"], ["p99", "p99"], ["errors", "Errors"], ["disk", "Disk"], ["lag", "Lag"]], lens, (v) => { lens = v; store.set("home.lens", v); writeHash(); drawWhere(); }, { "aria-label": "Colour by" });
  const groupSeg = seg([["shard", "Shard"], ["node", "Node"], ["zone", "Zone"]], group, (v) => { group = v; store.set("home.group", v); writeHash(); drawWhere(); }, { "aria-label": "Group by" });
  const whereTitle = h("h3", {}, "Where");
  const whereTools = h("div", { class: "tools" }, lensSeg, groupSeg, h("a", { class: "btn ghost sm", href: link("cluster") }, "Cluster", icon("right")));
  const whereCard = h("section", { class: "card span2 home-wherecard" },
    h("div", { class: "hd" }, whereTitle, whereSub, whereTools),
    h("div", { class: "bd" }, whereBody));
  // One server, not a cluster: no shards, copies or replicas to show, so
  // Home shows its tables instead (rows, disk, reads and writes), from
  // /v1/index/tables, read every 30 s.
  const one = () => !isCoordinator() && !(state.cluster?.shards || []).some((x) => (x.replicas || []).length > 1) && (state.cluster?.shards || []).length <= 1;
  let tix = null;
  const loadTix = () => one() ? api("/v1/index/tables", { timeout: 5000 }).then((j) => { tix = j.tables || []; drawWhere(); drawKpis(); }).catch(() => {}) : null;
  const probBody = h("div", { class: "stack home-probs" });
  const probTitle = h("h3", {}, "Problems");
  const probSub = h("span", { class: "sub" });
  const probCard = h("section", { class: "card home-probcard" }, h("div", { class: "hd" }, probTitle, probSub), h("div", { class: "bd" }, probBody));
  page.append(h("div", { class: "grid g3 home-row" }, whereCard, probCard));

  // Since when, and what changed.
  const brush = (a, b) => setZoom(a == null ? null : [a, b]);
  const markers = () => events.filter((e) => e.severity !== "info" || MARK.has(e.kind)).map((e) => ({ t: e.ts_ms, label: `${when(e.ts_ms)} ${e.title}` }));
  const errChart = timeChart({ height: 150, label: "error ratio", fmt: (v) => fmt.pct(v, v < 0.01 ? 2 : 1), series: [{ name: "error ratio", get: (p) => p.error_ratio ?? (p.qps ? (p.errors_s || 0) / p.qps : null), color: SERIES[7], area: true }], domain, onBrush: brush, markers, threshold: { value: SLO_ERR, label: "SLO" }, legend: false });
  const p99Chart = timeChart({ height: 150, label: "p99 latency", fmt: (v) => fmt.ms(v), series: [{ name: "p99", get: (p) => p.p99_ms, color: SERIES[6] }, { name: "p50", get: (p) => p.p50_ms, color: SERIES[0], dash: "3 3" }], domain, onBrush: brush, markers, threshold: { value: SLO_P99, label: "SLO" }, legend: false });
  const lane = h("div", { class: "home-lane" });
  const evList = h("div", { class: "home-evlist" });
  const chartsSub = h("span", { class: "sub" });
  const zoomLinks = h("div", { class: "tools" });
  const sinceTitle = h("h3", {}, "Since when, and what changed");
  const chartsCard = h("section", { class: "card home-since" },
    h("div", { class: "hd" }, sinceTitle, chartsSub, zoomLinks),
    h("div", { class: "bd" }, h("div", { class: "home-since-grid" },
      h("div", { class: "home-charts" }, h("div", { class: "small muted home-clabel" }, "error ratio"), errChart, h("div", { class: "small muted home-clabel" }, "p99 latency (dashed: p50)"), p99Chart, lane),
      h("div", { class: "home-evside" }, h("div", { class: "row nw home-evhead" }, h("b", {}, "What happened"), h("span", { class: "small muted" }, "newest first"), h("span", { class: "sep" }), h("a", { class: "small", href: link("timeline") }, "Timeline ", icon("right"))), evList))));
  page.append(chartsCard);

  // Capacity, coordinators, who is affected.
  const capBody = h("div", { class: "stack home-cap" });
  const coordBody = h("div");
  const whoBody = h("div");
  const whoSub = h("span", { class: "sub" }, "failing queries by table and by API key, this window");
  page.append(h("div", { class: "grid g3 home-row" },
    h("section", { class: "card home-capcard" }, h("div", { class: "hd" }, h("h3", {}, "Capacity"), h("span", { class: "sub" }, "copies now: worst and median"), h("div", { class: "tools" }, h("a", { class: "btn ghost sm", href: link("cluster", { tab: "capacity", range: "7d" }) }, "Trend 7 d", icon("right")))), h("div", { class: "bd" }, capBody)),
    h("section", { class: "card home-coordcard" }, h("div", { class: "hd" }, h("h3", {}, "Coordinators"), h("span", { class: "sub" }, "each one's own numbers")), h("div", { class: "bd flush" }, coordBody)),
    h("section", { class: "card home-whocard" }, h("div", { class: "hd" }, h("h3", {}, "Who is affected"), whoSub, h("div", { class: "tools" }, h("a", { class: "btn ghost sm", href: link("queries", { tab: "statements" }) }, "Queries", icon("right")))), h("div", { class: "bd" }, whoBody))));

  // ------------------------------------------------------------ data the view reads itself
  let events = [], eventsFrom = "events", eventsWhy = "", stats = null, statsKeys = false;
  const MARK = new Set(["copy_down", "copy_up", "primary_changed", "majority_lost", "majority_back", "rollout", "upgrade", "change", "alert_fired", "note", "moves_started", "contain"]);
  async function loadEvents() {
    const r = timeRange();
    try {
      const j = await api(`/v1/events?from=${Math.round(r.from)}&to=${Math.round(r.to)}&limit=500`, { timeout: 10000 });
      events = (j.events || []).slice().sort((a, b) => b.ts_ms - a.ts_ms);
      eventsFrom = "events";
    } catch (e) {
      // Refused (a key limited to tables): said, not shown as a quiet hour.
      if (e.status === 403) { events = []; eventsFrom = "refused"; eventsWhy = cannotSee(e, "the timeline"); drawEvents(); drawCharts(); return; }
      if (e.status !== 404 && e.status !== 405) return;
      // An older server: the alerts' own changes are what it remembers.
      eventsFrom = "alerts";
      events = (state.alerts?.events || []).filter((x) => x.ts_ms >= r.from && x.ts_ms <= r.to).map((x) => ({ id: `${x.rule || x.name}-${x.ts_ms}`, ts_ms: x.ts_ms, kind: x.state === "firing" ? "alert_fired" : x.state === "ok" ? "alert_resolved" : "alert_" + x.state, severity: x.state === "firing" ? (x.severity || "warning") : "info", title: `${x.name || x.rule}: ${x.state}`, where: { rule: x.rule } }));
    }
    drawEvents(); drawCharts();
  }
  async function loadStats() {
    const r = timeRange();
    try {
      const j = await api(`/v1/queries/stats?from=${Math.round(r.from)}&to=${Math.round(r.to)}&limit=500`, { timeout: 10000 });
      stats = Array.isArray(j) ? j : j.statements || [];
      statsKeys = stats.some((x) => Array.isArray(x.keys));
    } catch (e) { stats = e.status === 403 ? "forbidden" : null; }
    drawWho();
  }

  // ------------------------------------------------------------ head and banner
  function drawHead() {
    const m = model();
    const r = timeRange();
    const name = clusterName();
    const coords = state.fleet?.coordinators?.length || 0;
    // Copies a shard: one number when every shard has as many, else the range (NEW-scenarios-12).
    const ns = m.shards.map((s) => s.copies.length);
    const lo = Math.min(...ns), hi = Math.max(...ns);
    const copiesTxt = lo === hi ? ` × ${lo} ${lo === 1 ? "copy" : "copies"}` : `, ${lo}-${hi} copies`;
    const parts = [
      name || location.host,
      m.shards.length ? `${fmt.plural(m.shards.length, "shard")}${copiesTxt}${m.nodes.size ? ` on ${fmt.plural(m.nodes.size, "node")}` : ""}${m.zones.size ? ` in ${fmt.plural(m.zones.size, "zone")}` : ""}` : state.health ? (isCoordinator() ? "no shards listed" : "one server, not a cluster") : null,
      coords > 1 ? fmt.plural(coords, "coordinator") : null,
      `numbers over ${r.fixed ? span(r.from, r.to) : r.label.replace("last ", "the last ")} unless marked`,
    ].filter(Boolean);
    const seen = m.total ? `${m.up} of ${m.total} copies answered at the last look${m.at ? ", " + fmt.ago(m.at) : ""}` : "";
    const fr = fresh("cluster");
    clear(head, h("div", {}, h("h1", {}, one() ? "Health" : "Fleet health"), h("p", {}, parts.join(" · "))),
      h("div", { class: "tools" }, !fr.ok && !fr.loading ? h("span", { class: "stale-age" }, icon("clock"), staleText("cluster")) : null, seen ? h("span", { class: "small muted" }, seen) : null));
  }

  /** The tone of each headline card, so the banner is never greener than they are. */
  function cardTones() { return kpiDefs().map((d) => d.tone).filter(Boolean); }

  function drawBanner() {
    const f = state.fleet;
    const answering = !document.body.classList.contains("not-answering");
    if (!f && (state.fleetRefused || limited())) {
      banner.className = "banner home-banner unknown";
      clear(banner, h("div", { class: "bic" }, icon("lock")), h("div", { class: "grow" }, h("div", { class: "big" }, "This key can't see the fleet's health"),
        h("div", { class: "small" }, "It is limited to tables: the fleet, its events and its numbers show other tables' work. Its own tables are in ", h("a", { href: "#/sql" }, "SQL"), " and ", h("a", { href: "#/schema" }, "Schema"), ".")));
      return;
    }
    if (!f) { banner.className = "banner home-banner"; clear(banner, h("div", { class: "bic" }, h("span", { class: "spin" })), h("div", { class: "grow" }, h("div", { class: "big" }, "Checking the fleet…"))); return; }
    const tones = cardTones();
    let kind = f.status === "critical" ? "bad" : f.status === "warning" ? "warn" : "ok";
    if (tones.includes("bad")) kind = "bad"; else if (kind === "ok" && tones.includes("warn")) kind = "warn";
    if (!answering) kind = "unknown";
    banner.className = "banner home-banner " + kind;
    const probs = f.problems || [];
    const top = probs[0];
    let headline = f.headline || (top ? top.title : "Every check is within its limit");
    // The fleet said ok, a card says otherwise: say what the card says.
    if (f.status === "ok" && kind !== "ok" && kind !== "unknown") headline = kpiDefs().filter((d) => d.tone === kind).map((d) => d.why).filter(Boolean)[0] || "A headline number is over its limit";
    if (!answering) headline = "Not known: the server does not answer";
    // The headline problem's own since and first signal (NEW-shell-monitor-1); how long the
    // fleet has not been all right is said apart, when it began earlier.
    const since = top?.since_ms || f.since_ms || null;
    const first = top?.first_signal_ms ?? since;
    const notOk = f.not_ok_since_ms && since && f.not_ok_since_ms < since - 60e3 ? f.not_ok_since_ms : null;
    const rec = top?.id === "error_ratio_recovered" ? top : null;
    const past = isPast();
    if (past && answering) headline = "Now: " + headline;
    const sub = [];
    if (!answering) sub.push(`What shows is from ${when(f.at_ms)}. The last answer said: ${f.headline || "ok"}.`);
    else if (kind === "ok") {
      const m = model();
      sub.push([m.shards.length ? `${m.shards.filter((s) => s.st === "ok").length} of ${m.shards.length} shards serve reads and writes` : null, m.total ? `${m.up} of ${m.total} copies answer` : null,
        check("error_ratio")?.value != null ? `error ratio ${pctTxt(check("error_ratio").value)} (SLO ${pctTxt(sloErr())})` : null,
        p99Shown() != null ? `p99 ${fmt.ms(p99Shown())}${check("p99")?.recent?.value_ms != null ? " last minute" : ""} (SLO ${fmt.ms(sloP99())})` : null,
        firingNow().length ? null : "nothing firing", `checked ${when(f.at_ms)}`].filter(Boolean).join(" · "));
    } else {
      if (rec) sub.push(`failed ${when(rec.from_ms)}–${when(rec.to_ms)}; answering again since ${when(rec.to_ms)} (${durTxt(rec.to_ms)})`);
      else if (since) sub.push(`since ${when(since)} (${durTxt(since)})`);
      if (top?.impact) sub.push(top.impact);
      if (top?.where?.length) sub.push("where: " + top.where.join(", "));
      if (probs.length > 1) sub.push(`and ${fmt.plural(probs.length - 1, "other problem")}`);
    }
    const chips = [];
    if (past && answering) chips.push(h("span", { class: "home-chip warn", title: "The charts and events below are of the window picked; this banner, the numbers, Where and Problems are of now" }, `now, ${when(f.at_ms)}: the window shown is ${span(timeRange().from, timeRange().to)}`));
    if (kind !== "ok" && answering) {
      const fa = firingNow();
      if (first && !rec) chips.push(h("span", { class: "home-chip", title: "When this problem, or what it stems from, was first seen" }, `first signal ${when(first)}`));
      if (notOk) chips.push(h("span", { class: "home-chip", title: "Something has needed a look since then (other problems first)" }, `not all right since ${when(notOk)} (${durTxt(notOk)})`));
      for (const a of fa.slice(0, 2)) chips.push(h("a", { class: "home-chip", href: link("alert/" + encodeURIComponent(a.id)) }, `alert "${a.name}"${a.on ? " on " + a.on : ""} ${a.since_ms ? "since " + when(a.since_ms) : "firing"}`));
      const ack = fa.find((a) => a.ack);
      if (ack) chips.push(h("span", { class: "home-chip ack" }, `acked by ${ack.ack.by} ${when(ack.ack.at_ms)}`));
      else if (fa.length) chips.push(h("span", { class: "home-chip" }, "nobody has acked yet"));
    }
    const acts = [];
    if (top?.next && kind !== "ok" && answering) acts.push(h("a", { class: "btn sm primary", href: link(top.next.view, top.next.params || {}), title: top.next.text }, icon("right"), top.next.text.length > 42 ? nextShort(top.next) : top.next.text));
    // Logs since the problem began, live: a window that ends now does not pause the other views (NEW-scenarios-8).
    const logsFrom = rec ? rec.from_ms : since;
    if (kind !== "ok" && answering) acts.push(h("a", { class: "btn sm", href: logsLink({ level: "warn" }, logsFrom ? (rec ? { from: rec.from_ms - 60e3, to: rec.to_ms + 60e3 } : rangeSince(logsFrom - 60e3)) : {}), title: logsFrom ? `Warnings and errors from ${when(logsFrom - 60e3)}` : "Warnings and errors" }, icon("logs"), logsFrom ? (rec ? `Logs ${when(rec.from_ms - 60e3, { sec: false })}–${when(rec.to_ms + 60e3, { sec: false })}` : `Logs since ${when(logsFrom - 60e3, { sec: false })}`) : "Logs"));
    acts.push(h("button", { class: "btn sm", title: "Copy a link to Home at this window (opens paused)", onclick: () => copy(location.origin + location.pathname + link("home", rangeParams({ absolute: true })), "Link to Home at this window copied") }, icon("share"), "Copy link"));
    clear(banner,
      h("div", { class: "bic" }, icon(kind === "ok" ? "okc" : kind === "unknown" ? "info" : "warn", "lg")),
      h("div", { class: "grow" }, h("div", { class: "big" }, headline), sub.length ? h("div", { class: "small dim" }, sub.join(" · ")) : null, chips.length ? h("div", { class: "row home-chips" }, chips) : null,
        f.derived ? h("div", { class: "tiny muted", title: "This server has no /v1/health/fleet: the command center works it out from /v1/cluster, /v1/health, /v1/alerts and this coordinator's points" }, "Worked out in the browser from this coordinator's view: an older server") : null),
      h("div", { class: "row home-bacts" }, acts));
  }
  const VIEWNAME = { tables: "Tables and storage", inspector: "Table details", statement: "the statement", alerts: "Alerts", system: "Settings", sql: "SQL workbench", cluster: "Cluster", rollout: "Rollouts", logs: "Logs", queries: "Queries", performance: "Performance", autoscale: "Autoscaling", access: "Access", timeline: "Timeline", profile: "Profiler", home: "Home" };
  const nextShort = (n) => n.view === "alerts" ? "Open Alerts" : n.view.startsWith("alert/") ? "Open the alert" : n.params?.shard != null && n.view === "cluster" ? `Shard ${n.params.shard} in Cluster` : `Open in ${VIEWNAME[n.view] || n.view}`;

  // ------------------------------------------------------------ headline numbers
  function kpiDefs() {
    const m = model();
    const ps = visible();
    const lastP = ps[ps.length - 1] || {};
    const f = state.fleet;
    const er = check("error_ratio"), p9 = check("p99"), cp = check("copies"), lg = check("lag"), dk = check("disk"), shc = check("shards");
    const coords = f?.coordinators || [];
    const answered = coords.filter((c) => !c.error);
    const qpsFleet = coords.length > 1 ? answered.reduce((a, c) => a + (c.qps || 0), 0) : lastP.qps ?? coords[0]?.qps ?? null;
    const now = isPast() ? "now · " : "";
    const tone = (st) => st === "critical" ? "bad" : st === "warning" ? "warn" : "";
    const out = [];
    const dark = m.shards.filter((s) => ["no_reads", "no_writes", "down"].includes(s.st));
    if (dark.length) {
      // "Fully serving": a shard that answers searches but takes no writes is not counted.
      out.push({ key: "shards", label: "Shards fully serving", icon: "shard", val: [String(m.shards.length - dark.length), h("small", {}, "/" + m.shards.length)], tone: "bad", sub: dark.slice(0, 3).map((s) => `shard ${s.n} ${s.st === "no_reads" && s.sh.majority === false ? "has no majority" : stText(s.st)}`).join(", "), why: `${fmt.plural(dark.length, "shard")} serve no reads or take no writes`, link: link("cluster", { shard: dark[0].n }) });
    } else {
      out.push({ key: "qps", label: coords.length > 1 ? "Queries, fleet" : "Queries", icon: "queries", val: [fmt.si(qpsFleet), h("small", {}, "/s")], spark: ps.map((p) => p.qps), color: SERIES[0],
        sub: coords.length > 1 ? `${now}${answered.length} of ${coords.length} coordinators, over 5 min` : f?.derived ? "this coordinator, now" : one() ? `${now}this server, now` : `${now}its one coordinator, now`, link: link("performance") });
    }
    // The last minute when the server says it (NEW-scenarios-2), else the window.
    const p99Recent = p9?.recent?.value_ms != null;
    const p99v = p99Shown() ?? lastP.p99_ms ?? null;
    const lastMin = (w) => w ? `last ${w >= 120 ? fmt.dur(w).replace(/m 0s$/, " min") : "minute"}` : "";
    const p99Tone = p9 ? (p9.status === "warning" && p99Recent && p99v <= sloP99() ? "" : tone(p9.status)) : p99v > sloP99() ? "bad" : "";
    out.push({ key: "p99", label: "p99 latency", icon: "clock", val: msParts(p99v), tone: p99Tone, spark: ps.map((p) => p.p99_ms), color: SERIES[6], sub: `${now}SLO ${fmt.ms(sloP99())} · ${p99Recent ? lastMin(p9.recent.window_s) : p9?.window_s ? "over " + fmt.dur(p9.window_s).replace(/m 0s$/, " min") : ""}`, why: p99v != null ? `p99 ${fmt.ms(p99v)} is over the SLO (${fmt.ms(sloP99())})` : null, link: link("performance") });
    const erRecent = er?.recent && er.recent.value !== undefined && (er.recent.queries ?? 1) > 0;
    const erv = erRecent ? er.recent.value : er?.value ?? null;
    const failing = erv != null && qpsFleet ? erv * qpsFleet : null;
    // A failure that ended: the card says now's value, and when it recovered.
    const recovered = er?.recovered;
    const erTone = er ? (recovered && !(erv > sloErr()) ? "" : tone(er.status)) : "";
    const erSub = recovered && !(erv > sloErr()) ? `recovered at ${when(recovered.to_ms, { sec: false })} after ${durOf(recovered.to_ms - recovered.from_ms + 2000)}, ${pctTxt(recovered.worst)} at worst` : `SLO ${pctTxt(sloErr())} · ${erRecent ? lastMin(er.recent.window_s) : "over 5 min"}${failing != null && failing >= 0.1 ? ` · ${fmt.si(failing)} q/s failing` : erv == null ? " · no query ended" : ""}`;
    out.push({ key: "err", label: "Error ratio", icon: "warn", val: erv == null ? ["–"] : [pctTxt(erv).replace("%", ""), h("small", {}, "%")], tone: erTone, spark: ps.map((p) => p.error_ratio ?? null), color: SERIES[7], sub: now + erSub, why: erv != null ? `${pctTxt(erv)} of queries failed over the ${erRecent ? "last minute" : "last 5 min"} (SLO ${pctTxt(sloErr())})` : null, link: link("queries", { tab: "statements", outcome: "error" }) });
    const downNames = (cp?.where || m.shards.flatMap((s) => s.copies.filter((x) => !x.up).map((x) => x.name)));
    const tot = cp?.total ?? m.total, upN = cp ? cp.total - cp.down : m.up;
    out.push({ key: "copies", label: "Copies answering", icon: "db", val: tot ? [String(upN), h("small", {}, "/" + tot)] : ["–"], tone: cp ? tone(cp.status) : upN < tot ? "warn" : "", sub: downNames.length ? `${downNames.slice(0, 3).join(", ")}${downNames.length > 3 ? ` +${downNames.length - 3}` : ""} down` : tot ? "every copy answers" : "no copies listed", why: downNames.length ? `${fmt.plural(downNames.length, "copy", "copies")} do not answer` : null, link: link("cluster") });
    const lagV = lg?.max_txn ?? null;
    out.push({ key: "lag", label: "Replication lag, max", icon: "layers", val: lagV == null ? ["–"] : [fmt.si(lagV), h("small", {}, "txn")], tone: lg ? tone(lg.status) : "", sub: lagV == null ? "no replica reports one" : lagV === 0 ? "every replica in step" : `${lg.where || ""}${lg.max_s != null ? ` · reads up to ${fmt.dur(lg.max_s)} old` : ""}`, why: lagV ? `${lg.where} is ${fmt.si(lagV)} transactions behind` : null, link: link("cluster", { lens: "lag" }) });
    if (one() && tix) {
      // One server: copies and replicas say nothing; its rows and writes do.
      const rows = tix.reduce((a, x) => a + (x.docs || 0), 0), bytes = tix.reduce((a, x) => a + (x.bytes || 0), 0);
      const wr = tix.reduce((a, x) => a + (x.writes_per_min || 0), 0), rd = tix.reduce((a, x) => a + (x.reads_per_min || 0), 0);
      const lastW = Math.max(0, ...tix.map((x) => x.last_write_ms || 0));
      const i = out.findIndex((d) => d.key === "copies");
      out.splice(i, 2,
        { key: "rows", label: "Rows", icon: "table", val: [fmt.si(rows)], sub: `${fmt.plural(tix.length, "table")} · ${fmt.bytes(bytes)} on disk`, link: "#/table" },
        { key: "rw", label: "Reads and writes", icon: "db", val: [fmt.si(rd), h("small", {}, "/min")], sub: `${fmt.si(wr)} writes/min${lastW ? ` · last write ${fmt.ago(lastW)}` : ""}`, link: "#/table" });
    }
    const dkV = dk?.max ?? null;
    out.push({ key: "disk", label: "Fullest disk", icon: "disk", val: dkV == null ? ["–"] : [fmt.n(dkV * 100, 0), h("small", {}, "%")], tone: dk ? tone(dk.status) : "", sub: dkV == null ? "not known" : `${now}${dk.where || ""}${dk.over?.length > 1 ? ` and ${dk.over.length - 1} more over 85%` : ""}${dk.days_to_full != null ? ` · full in about ${fmt.fills(dk.days_to_full)}` : ""}${dk.median != null ? ` · median ${fmt.pct(dk.median, 0)}` : ""}`, why: dkV != null ? `${dk.where}'s disk is ${fmt.pct(dkV, 0)} full` : null, link: link("cluster", { tab: "capacity" }) });
    if (shc && !dark.length && shc.status !== "ok") out[0].tone = tone(shc.status);
    return out;
  }
  function drawKpis() {
    const fr = fresh("fleet");
    clear(kpiRow, kpiDefs().map((d) => {
      const c = h("a", { class: "card kpi home-kpi " + (d.tone || "") + (!fr.ok && !fr.loading ? " stale" : ""), href: d.link, title: d.why || "" },
        h("div", { class: "lbl" }, icon(d.icon), d.label), h("div", { class: "val" }, d.val), h("div", { class: "delta" }, d.sub || " "));
      if (d.spark && d.spark.filter((x) => x != null).length > 1) c.append(sparkline(d.spark.slice(-180), { color: d.tone === "bad" ? "var(--crit)" : d.tone === "warn" ? "var(--warn)" : d.color }));
      return c;
    }));
  }

  // ------------------------------------------------------------ where
  let lastModel = null, showAll = false;
  function drawTables() {
    whereTitle.textContent = "Tables";
    whereTools.style.display = "none";
    if (!tix) { clear(whereBody, empty("table", "Reading the tables…")); whereSub.textContent = ""; return; }
    const list = tix.slice().sort((a, b) => (b.reads_per_min || 0) - (a.reads_per_min || 0) || (b.docs || 0) - (a.docs || 0));
    const mine = list.filter((x) => isMine(x.table));
    const shown = (mine.length ? mine : list).slice(0, 8);
    whereSub.textContent = `${fmt.plural(tix.length, "table")}${mine.length !== tix.length ? `, ${mine.length} yours` : ""} · busiest first, over the last 5 minutes`;
    clear(whereBody, shown.length ? [h("div", { class: "tbl-wrap" }, h("table", { class: "t compact" },
      h("thead", {}, h("tr", {}, h("th", {}, "Table"), h("th", { class: "num" }, "Rows"), h("th", { class: "num" }, "On disk"), h("th", { class: "num" }, "Reads/min"), h("th", { class: "num" }, "Writes/min"), h("th", { class: "num" }, "Last write"))),
      h("tbody", {}, shown.map((x) => h("tr", { class: "click", onclick: () => { location.hash = `#/table/${encodeURIComponent(x.table)}`; } },
        h("td", {}, h("a", { class: "mono", href: `#/table/${encodeURIComponent(x.table)}` }, x.table)), h("td", { class: "num" }, fmt.n(x.docs)), h("td", { class: "num" }, fmt.bytes(x.bytes)),
        h("td", { class: "num" }, fmt.si(x.reads_per_min || 0)), h("td", { class: "num" }, fmt.si(x.writes_per_min || 0)), h("td", { class: "num muted" }, x.last_write_ms ? fmt.ago(x.last_write_ms) : "–")))))),
      h("div", { class: "row small", style: { marginTop: "8px" } }, h("a", { href: "#/table" }, "Every table", icon("right")), h("span", { class: "sep" }), h("span", { class: "muted" }, "One server: its health is the numbers above."))] : empty("table", "No tables yet", "Import a file to make one.", h("a", { class: "btn", href: "#/start" }, icon("rocket"), "Start here")));
  }
  function drawWhere() {
    if (one()) { if (!tix) loadTix(); return drawTables(); }
    whereTitle.textContent = "Where";
    whereTools.style.display = "";
    const m = lastModel = model();
    whereCard.classList.toggle("stale", !fresh("cluster").ok && !fresh("cluster").loading);
    if (!m.shards.length) {
      clear(whereBody, state.cluster === null && !isCoordinator() ? empty("db", "One server", "This server is not a coordinator: it has no shards to show. Its health is in the numbers above.") : empty("db", state.cluster ? "No shards listed" : "Reading the cluster…"));
      whereSub.textContent = "";
      return;
    }
    whereSub.textContent = group === "shard" ? `${fmt.plural(m.shards.length, "shard")}, each its worst copy now · no colour = fine` : group === "node" ? (m.nodes.size ? `${fmt.plural(m.total, "copy", "copies")} on ${fmt.plural(m.nodes.size, "node")}` : "nodes not known: grouped by address") : (m.zones.size ? `${fmt.plural(m.zones.size, "zone")}` : "zones not known");
    const legend = h("div", { class: "row home-legend small muted" },
      h("span", {}, h("i", { class: "lg" }), " fine"), h("span", {}, h("i", { class: "lg warn" }), " needs a look"), h("span", {}, h("i", { class: "lg crit" }), " hurts queries now"), h("span", {}, h("i", { class: "pip down" }), " copy down"), h("span", {}, h("i", { class: "pip p" }), " primary"),
      h("span", { class: "sep" }), h("span", {}, "click a shard: its copies, logs and profile for this window"));
    if (group === "shard") {
      // On a phone a large fleet shows the shards that need a look first; all of them on request.
      const few = isPhone() && !showAll && m.shards.length > 24;
      const list = few ? m.shards.filter((s) => shardCell(s, lens).tone) : m.shards;
      const big = list.length <= 16;
      const grid = list.length ? h("div", { class: "home-heat" + (big ? " big" : "") }, list.map((s) => shardEl(s, big))) : null;
      const toggleAll = h("button", { class: "btn sm", onclick: () => { showAll = !showAll; drawWhere(); } }, showAll ? "Only the ones that need a look" : `Show all ${m.shards.length} shards`);
      const note = isPhone() && m.shards.length > 24 ? h("div", { class: "row home-few small dim" }, few ? (list.length ? `${fmt.plural(list.length, "shard")} of ${m.shards.length} need a look; the others are fine.` : `All ${m.shards.length} shards are fine by this lens.`) : `All ${m.shards.length} shards.`, h("span", { class: "sep" }), toggleAll) : null;
      clear(whereBody, note, grid, legend, zoneCards(m));
    } else {
      const groups = group === "node" ? (m.nodes.size ? m.nodes : byHost(m)) : m.zones;
      if (!groups.size) { clear(whereBody, empty("cloud", "Zones not known", "Each copy says its zone when the server runs with CHLOEDB_ZONE set (the operator sets it on Kubernetes)."), legend); return; }
      const list = [...groups.entries()].map(([k, xs]) => ({ k, xs, bad: xs.filter((x) => copyTone(x) === "crit").length, warn: xs.filter((x) => copyTone(x) === "warn").length }))
        .sort((a, b) => (b.bad - a.bad) || (b.warn - a.warn) || String(a.k).localeCompare(String(b.k), undefined, { numeric: true }));
      clear(whereBody, h("div", { class: "home-groups" + (group === "zone" ? " zones" : "") }, list.map((g) => h("div", { class: "home-group" + (g.bad ? " crit" : g.warn ? " warn" : "") },
        h("div", { class: "row nw small" }, h("b", { class: "ellipsis", title: g.k }, group === "zone" ? "zone " + g.k : g.k), h("span", { class: "sep" }), h("span", { class: "muted" }, `${g.xs.filter((x) => x.up).length}/${g.xs.length}`)),
        h("div", { class: "home-copies" }, g.xs.sort((a, b) => a.shard - b.shard || a.j - b.j).map((x) => copyEl(x)))))), legend);
    }
  }
  function byHost(m) { const g = new Map(); for (const s of m.shards) for (const x of s.copies) { const k = x.host.replace(/:\d+$/, ""); if (!g.has(k)) g.set(k, []); g.get(k).push(x); } return g; }
  function shardEl(s, big) {
    const c = shardCell(s, lens);
    const el = h("button", { class: "hc " + c.tone, "aria-label": `shard ${s.n}: ${c.value}` },
      h("span", { class: "n" }, String(s.n)), c.tag ? h("em", {}, c.tag) : null,
      big ? h("span", { class: "names" }, s.copies.map((x) => h("span", { class: !x.up ? "down" : copyTone(x) || "", title: x.up ? null : "not answering" }, x.name))) : null,
      h("span", { class: "pips" }, s.copies.map((x) => h("i", { class: "pip" + (x.primary ? " p" : "") + (!x.up ? " down" : copyTone(x) ? " " + copyTone(x) : "") }))));
    tipOn(el, () => [h("div", { class: "t" }, `Shard ${s.n} · ${stText(s.st)}`), h("div", { class: "small", style: { marginBottom: "4px" } }, c.value),
      ...s.copies.map((x) => h("div", { class: "r" }, h("span", { class: "k box", style: { background: !x.up ? "var(--crit)" : copyTone(x) === "warn" ? "var(--warn)" : copyTone(x) === "crit" ? "var(--crit)" : "var(--good)" } }), h("b", {}, x.name), h("span", { class: "n" }, [x.up ? x.role : "not answering", x.p99 != null ? "p99 " + fmt.ms(x.p99) : null, x.disk != null ? "disk " + fmt.pct(x.disk, 0) : null, x.lag ? fmt.si(x.lag) + " behind" : null].filter(Boolean).join(" · "))))]);
    el.onclick = () => openShard(s);
    return el;
  }
  function copyEl(x) {
    const t = copyTone(x);
    const el = h("button", { class: "hcopy " + t + (x.primary ? " p" : "") + (!x.up ? " down" : ""), title: `${copyTitle(x.url)}\n${x.up ? x.role : "not answering"}${x.p99 != null ? " · p99 " + fmt.ms(x.p99) : ""}${x.disk != null ? " · disk " + fmt.pct(x.disk, 0) : ""}${x.lag ? " · " + fmt.si(x.lag) + " behind" : ""}` }, x.name);
    el.onclick = () => openShard(lastModel.shards.find((s) => s.n === x.shard));
    return el;
  }
  function zoneCards(m) {
    if (!m.zones.size) return null;
    return h("div", { class: "home-zones" }, [...m.zones.entries()].sort().map(([z, xs]) => {
      const down = xs.filter((x) => !x.up);
      const prim = xs.filter((x) => x.primary && copyTone(x) === "crit");
      return h("a", { class: "home-zone" + (down.length ? " crit" : ""), href: link("cluster", { group: "zone", zone: z }) }, h("b", {}, "zone " + z),
        h("span", { class: "small dim" }, `${xs.length - down.length}/${xs.length} copies answer`),
        h("span", { class: "small muted ellipsis" }, down.length ? `${down.length} down · ${down.slice(0, 3).map((x) => x.name).join(", ")}` : "0 down" + (prim.length ? ` · ${prim[0].name} primary here` : "")));
    }));
  }
  function openShard(s) {
    if (!s) return;
    const r = timeRange();
    const prim = s.copies.find((x) => x.primary);
    const rows = s.copies.map((x) => h("tr", {},
      h("td", {}, h("span", { class: "dot " + (x.up ? copyTone(x) === "crit" ? "bad" : copyTone(x) === "warn" ? "warn" : "ok" : "bad") }), " ", h("b", { title: copyTitle(x.url) }, x.name)),
      h("td", {}, x.up ? x.role : `not answering${x.down_since ? " since " + when(x.down_since) : ""}`), h("td", { class: "small dim" }, [x.zone ? "zone " + x.zone : null, x.node].filter(Boolean).join(" · ") || x.host),
      h("td", { class: "num", "data-l": "p99" }, x.p99 != null ? fmt.ms(x.p99) : "–"), h("td", { class: "num", "data-l": "disk" }, x.disk != null ? fmt.pct(x.disk, 0) : "–"), h("td", { class: "num", "data-l": "behind" }, x.lag != null ? fmt.si(x.lag) : "–"),
      h("td", {}, h("a", { class: "btn xs", href: logsLink({ server: x.url }) }, "Logs"), " ", x.up ? h("a", { class: "btn xs", href: link("profile", { tab: "cpu", node: x.url }) }, "Profile") : null)));
    const d = drawer({
      title: `Shard ${s.n}`, sub: `${stText(s.st)} · ${span(r.from, r.to)}`,
      body: [
        s.why || s.st !== "ok" ? h("div", { class: s.st === "ok" ? "notice" : ["no_reads", "no_writes", "down"].includes(s.st) ? "error" : "notice" }, s.why || `${s.copies.filter((x) => x.up).length} of ${s.copies.length} copies answer.`) : null,
        h("div", { class: "tbl-wrap" }, h("table", { class: "t compact home-copytbl" }, h("thead", {}, h("tr", {}, ["Copy", "Role", "Where", "p99", "Disk", "Behind", ""].map((x, i) => h("th", { class: i >= 3 && i <= 5 ? "num" : "" }, x)))), h("tbody", {}, rows))),
        h("div", { class: "row" },
          h("a", { class: "btn primary", href: link("cluster", { shard: s.n }) }, icon("cluster"), "Shard " + s.n + " in Cluster"),
          h("a", { class: "btn", href: logsLink({ shard: s.n, level: "warn" }) }, icon("logs"), "Its logs, this window"),
          prim ? h("a", { class: "btn", href: link("profile", { tab: "cpu", node: prim.url }) }, icon("flame"), `Profile ${prim.name}`) : null,
          h("a", { class: "btn", href: link("timeline", { shard: s.n }) }, icon("timeline"), "Its events")),
      ],
    });
    d.el.addEventListener("click", (e) => { if (e.target.closest("a[href^='#']")) d.close(); });
  }

  // ------------------------------------------------------------ problems
  function drawProblems() {
    const f = state.fleet;
    probCard.classList.toggle("stale", !fresh("fleet").ok && !fresh("fleet").loading);
    if (!f) { clear(probBody, h("div", { class: "muted small" }, "Checking…")); return; }
    const probs = f.problems || [];
    const restore = probs[0]?.severity === "critical" && /majority|dark|does not answer|no reads/i.test(probs[0].title);
    probTitle.textContent = restore ? "What would restore service" : "Problems";
    probSub.textContent = probs.length ? `${probs.length}, worst first · where, since when, next step${isPast() ? " · now, not the window shown" : ""}` : "";
    if (!probs.length) {
      const m = model();
      const okRows = [
        m.shards.length ? `${m.shards.length} of ${m.shards.length} shards serve reads and writes` : null,
        m.total ? `${m.up} of ${m.total} copies answer` : null,
        check("error_ratio")?.value != null ? `${pctTxt(check("error_ratio").value)} of queries fail (SLO ${pctTxt(sloErr())})` : "no failed queries counted",
        p99Shown() != null ? `p99 ${fmt.ms(p99Shown())}${check("p99")?.recent?.value_ms != null ? " last minute" : ""} (SLO ${fmt.ms(sloP99())})` : null,
        check("lag") ? `replication lag at most ${fmt.si(check("lag").max_txn || 0)} transactions` : null,
        check("disk")?.max != null ? `fullest disk ${fmt.pct(check("disk").max, 0)}` : null,
        `${firingNow().length ? fmt.plural(firingNow().length, "alert") + " firing" : "no alert firing"}`,
      ].filter(Boolean);
      clear(probBody, h("div", { class: "row nw", style: { gap: "10px" } }, h("span", { style: { color: "var(--good)" } }, icon("okc", "lg")), h("b", {}, "No problems")),
        h("ul", { class: "home-oklist" }, okRows.map((t) => h("li", {}, icon("check"), t))),
        h("div", { class: "small muted" }, `As of ${when(f.at_ms)}. What changed recently is on the `, h("a", { href: link("timeline") }, "Timeline"), "."));
      return;
    }
    const upUrls = new Set(model().shards.flatMap((s) => s.copies.filter((x) => x.up).map((x) => x.url)));
    clear(probBody, probs.slice(0, 8).map((p) => {
      const actions = [];
      if (p.next) actions.push(h("a", { class: "btn xs primary", href: link(p.next.view, p.next.params || {}), title: p.next.text }, nextShort(p.next)));
      const copyUrl = (p.where || []).map((w) => findUrl(w)).find(Boolean);
      const shardW = (p.where || []).map((w) => /^shard (\d+)$/.exec(w)).find(Boolean);
      const pFrom = p.from_ms ?? p.since_ms;
      actions.push(h("a", { class: "btn xs", href: logsLink({ ...(copyUrl ? { server: copyUrl } : shardW ? { shard: shardW[1] } : {}), level: "warn" }, pFrom ? (p.to_ms ? { from: pFrom - 60e3, to: p.to_ms + 60e3 } : rangeSince(pFrom - 60e3)) : {}), title: pFrom ? `Warnings and errors from ${when(pFrom - 60e3)}` : null }, "Logs"));
      const upCopy = copyUrl && upUrls.has(copyUrl);
      if (upCopy && p.id !== "copies-down") actions.push(h("a", { class: "btn xs", href: link("profile", { tab: "cpu", node: copyUrl }) }, "Profile " + copyName(copyUrl)));
      return h("div", { class: "prob " + (p.severity === "critical" ? "crit" : "warn") },
        h("span", { class: "sv" }),
        h("div", { class: "grow" },
          h("div", { class: "pt" }, p.title),
          p.where?.length ? h("div", { class: "small dim" }, p.where.join(" · ")) : null,
          h("div", { class: "small dim" }, [p.to_ms ? h("span", { title: whenTitle(p.from_ms) }, `${when(p.from_ms)}–${when(p.to_ms)}, answering again for ${durTxt(p.to_ms)}`) : p.since_ms ? h("span", { title: whenTitle(p.since_ms) }, `since ${when(p.since_ms)} (${durTxt(p.since_ms)})`) : null, p.impact].filter(Boolean).flatMap((x, i) => i ? [" · ", x] : [x])),
          p.next?.text ? h("div", { class: "small home-next" }, icon("right"), p.next.text, p.next.runbook ? [" · runbook: ", h("a", { href: RUNBOOKS + p.next.runbook.toLowerCase().replace(/[^a-z0-9 -]/g, "").trim().replace(/\s+/g, "-"), target: "_blank", rel: "noopener" }, p.next.runbook, " ", icon("ext"))] : null) : null,
          h("div", { class: "row home-pacts" }, actions)));
    }), probs.length > 8 ? h("div", { class: "small muted" }, `and ${probs.length - 8} more`) : null);
  }
  function findUrl(name) {
    for (const s of model().shards) for (const x of s.copies) if (x.name === name || x.url === name) return x.url;
    return null;
  }

  // ------------------------------------------------------------ since when
  function drawCharts() {
    const ps = visible();
    const slo = sloErr(), slop = sloP99();
    errChart.setThreshold(slo); p99Chart.setThreshold(slop);
    errChart.update(ps); p99Chart.update(ps);
    chartsCard.classList.toggle("stale", !fresh("series").ok && !fresh("series").loading);
    const coords = state.fleet?.coordinators?.length || 1;
    const r = timeRange();
    const src = state.series;
    // Where the stored history begins, when the window starts before it (NEW-shell-monitor-2).
    const [d0] = domain();
    const from = ps.length && ps[0].t - Math.min(d0, r.zoomed ? d0 : r.from) > 5 * 60e3 ? `data from ${when(ps[0].t, { sec: false })}` : null;
    chartsSub.textContent = [coords > 1 ? "the queries this coordinator answered (each coordinator's own are below)" : "the queries this server answered", from,
      `with the fleet's changes in ${r.zoomed ? "the zoom" : "the window"}`, src.source === "load" ? "a point a minute" : `a point every ${fmt.dur(state.interval / 1000)}`,
      src.partial ? "this server keeps only its last hour" : null, "drag to zoom; the zoom goes into the links"].filter(Boolean).join(" · ");
    const q = rangeParams({ absolute: r.zoomed });
    clear(zoomLinks,
      h("a", { class: "btn ghost sm", href: logsLink({ level: "warn" }, q) }, r.zoomed ? "Logs for the zoom" : "Logs for the window", icon("right")),
      h("a", { class: "btn ghost sm", href: link("queries", { tab: "statements", ...q }) }, r.zoomed ? "Queries for the zoom" : "Queries for the window", icon("right")));
    drawLane();
  }
  function drawLane() {
    const [t0, t1] = domain();
    const evs = events.filter((e) => e.ts_ms >= t0 && e.ts_ms <= t1 && (e.severity !== "info" || MARK.has(e.kind)));
    // The chart's plot starts 46 px in and ends 14 px before the edge. Events
    // closer than 12 px are one label (as on the charts); a label takes the
    // first of three rows with room, and what finds none is counted on the
    // label to its left, so labels never print over each other.
    const W = Math.max(120, (lane.clientWidth || 600) - 60);
    const piles = [];
    for (const e of evs.slice(0, 300).sort((a, b) => a.ts_ms - b.ts_ms)) {
      const x = (e.ts_ms - t0) / Math.max(1, t1 - t0) * W, p = piles[piles.length - 1];
      if (p && x - p.x < 12) p.evs.push(e); else piles.push({ x, evs: [e] });
    }
    const ends = [-1e9, -1e9, -1e9], shown = [];
    for (const p of piles) {
      const first = p.evs.find((e) => e.severity === "critical") || p.evs.find((e) => e.severity === "warning") || p.evs[0];
      const width = Math.min(260, 40 + first.title.length * 5.6 + (p.evs.length > 1 ? 60 : 0));
      // Near the right edge the label goes to the left of its line.
      const left = p.x + width > W + 10 && p.x > width;
      const x0 = left ? p.x - width : p.x;
      const row = ends.findIndex((end) => end <= x0);
      if (row < 0) { if (shown.length) shown[shown.length - 1].more.push(...p.evs); continue; }
      ends[row] = x0 + width + 8;
      shown.push({ ...p, row, first, width, left, x0, more: [] });
    }
    const cls = (e) => e.severity === "critical" ? "crit" : e.severity === "warning" ? "warn" : MARK.has(e.kind) ? "accent" : "";
    clear(lane, shown.map((p) => {
      const all = [...p.evs, ...p.more], n = all.length;
      const lines = all.slice(0, 12).map((e) => `${whenTitle(e.ts_ms)} ${e.title}${e.who ? " (by " + e.who + ")" : ""}`);
      return h("a", { class: "ev " + cls(p.first) + (p.left ? " left" : ""), href: n > 1 ? link("timeline", { from: Math.round(all[0].ts_ms - 5 * 60e3), to: Math.round(Math.min(Date.now(), all[n - 1].ts_ms + 5 * 60e3)) }) : eventLink(p.first),
        title: lines.join("\n") + (n > 12 ? `\nand ${n - 12} more` : "") + (n > 1 ? "\nClick for the Timeline around them." : ""),
        style: { left: `calc(46px + ${p.x0.toFixed(1)}px)`, top: p.row * 15 + "px", ...(p.left ? { width: p.width + "px" } : { maxWidth: p.width + "px" }) } },
        h("b", {}, when(p.first.ts_ms, { sec: false })), " ", n > 1 ? h("span", { class: "ev-n" }, `${n} changes: `) : null, p.first.title);
    }));
    if (!evs.length) lane.append(h("div", { class: "small muted", style: { paddingLeft: "46px", paddingTop: "4px" } }, eventsFrom === "refused" ? eventsWhy : eventsFrom === "events" ? "No change in this window: no copy went down, no primary moved, nobody changed anything." : "This server keeps no events (it has no /v1/events): only alert changes show."));
  }
  function drawEvents() {
    const evs = events.slice(0, 12);
    if (!evs.length && eventsFrom === "refused") { clear(evList, h("div", { class: "small muted" }, eventsWhy)); return; }
    if (!evs.length) { clear(evList, h("div", { class: "small muted" }, eventsFrom === "events" ? `Nothing happened in ${timeRange().label.replace("last", "the last")}.` : "This server keeps no events: an older server.")); return; }
    clear(evList, evs.map((e) => h("a", { class: "home-ev", href: eventLink(e), title: whenTitle(e.ts_ms) },
      h("span", { class: "mono small" }, when(e.ts_ms)), h("span", { class: "home-kind " + kindTone(e) }, kindName(e.kind)), h("span", { class: "small" }, e.title, e.who ? h("span", { class: "muted" }, " · " + e.who) : null))));
  }

  // ------------------------------------------------------------ capacity
  function drawCapacity() {
    const m = model();
    const xs = m.shards.flatMap((s) => s.copies).filter((x) => x.up);
    const dk = check("disk");
    const row = (label, f, extra) => {
      const vs = xs.map((x) => ({ x, v: f(x) })).filter((a) => a.v != null);
      if (!vs.length) return h("div", { class: "home-caprow" }, h("div", { class: "row nw" }, h("b", {}, label), h("span", { class: "sep" }), h("span", { class: "small muted" }, "not known")));
      const worst = vs.reduce((a, b) => (b.v > a.v ? b : a));
      const med = median(vs.map((a) => a.v));
      const tone = worst.v > 0.9 ? "crit" : worst.v > 0.75 ? "warn" : "";
      return h("div", { class: "home-caprow" },
        h("div", { class: "row nw small" }, h("b", { class: "lbl" }, label), h("span", {}, `worst ${fmt.pct(worst.v, 0)} · `, h("span", { title: copyTitle(worst.x.url) }, worst.x.name), extra ? " · " + extra : ""), h("span", { class: "sep" }), h("span", { class: "muted" }, `median ${fmt.pct(med, 0)}`)),
        h("div", { class: "home-meter " + tone }, h("i", { style: { width: Math.min(100, worst.v * 100) + "%" } }), h("b", { class: "med", style: { left: Math.min(100, med * 100) + "%" }, title: "median" })));
    };
    if (!xs.length) { clear(capBody, h("div", { class: "small muted" }, isCoordinator() || state.cluster ? "No copy reports its load." : "One server: its own numbers are on System.")); return; }
    clear(capBody,
      row("CPU", (x) => x.cpu),
      row("Disk", (x) => x.disk, dk?.days_to_full != null ? `full in about ${fmt.fills(dk.days_to_full)}` : null),
      row("Memory", (x) => x.mem),
      row("Query slots", (x) => x.slots));
  }

  // ------------------------------------------------------------ coordinators
  function drawCoords() {
    const f = state.fleet;
    const cs = f?.coordinators || [];
    if (!cs.length) { clear(coordBody, h("div", { class: "small muted", style: { padding: "0 14px 14px" } }, f ? "No coordinator listed: this server is not one." : "Checking…")); return; }
    // Each coordinator's own numbers over 5 min; errors over the last minute when it says (OV-4).
    const errOf = (c) => c.recent_error_ratio ?? c.error_ratio;
    const t = h("table", { class: "t compact home-coordtbl" }, h("thead", {}, h("tr", {}, ["Coordinator", "q/s", "p99", "Errors", "CPU"].map((x, i) => h("th", { class: i ? "num" : "" }, x)))),
      h("tbody", {}, cs.map((c) => h("tr", {},
        h("td", {}, h("span", { class: "dot " + (c.error ? "warn" : (errOf(c) ?? 0) > sloErr() || (c.p99_ms ?? 0) > sloP99() ? "bad" : "ok") }), " ", c.url && !c.this ? h("a", { href: c.url.replace(/\/$/, "") + "/#/home", title: `${c.url}: open its command center` }, c.name || hostOf(c.url)) : h("span", { title: c.url || "" }, c.name || hostOf(c.url)), c.this ? h("span", { class: "muted" }, " (this)") : null),
        ...(c.error ? [h("td", { class: "small muted", colspan: 4, title: c.error }, "did not answer: " + c.error.replace(/^[^:]*: /, ""))] : [
          h("td", { class: "num" }, fmt.si(c.qps)), h("td", { class: "num" }, c.p99_ms != null ? fmt.ms(c.p99_ms) : "–"),
          h("td", { class: "num " + ((errOf(c) ?? 0) > sloErr() ? "home-bad" : ""), title: c.recent_error_ratio != null ? `last minute; ${pctTxt(c.error_ratio)} over 5 min` : "over 5 min" }, pctTxt(errOf(c))), h("td", { class: "num" }, c.cpu != null ? (c.cpu > 1.5 ? fmt.si(c.cpu) + " cores" : fmt.pct(c.cpu, 0)) : "–")])))));
    let note = null;
    const ers = cs.filter((c) => !c.error).map(errOf).filter((x) => x != null);
    if (cs.length === 1) note = f.derived ? "This server does not list the other coordinators (an older server)." : "The only coordinator the shards heard from in the last minute: its numbers are the fleet's.";
    else if (ers.length === cs.length && Math.max(...ers) > sloErr()) {
      const lo = Math.min(...ers), hi = Math.max(...ers);
      note = hi < lo * 1.5 ? `The same error ratio on all ${cs.length}: not a coordinator problem.` : `${cs.find((c) => errOf(c) === hi).name || "One coordinator"} fails more than the others: look at it first.`;
    } else if (cs.some((c) => c.error)) note = `${fmt.plural(cs.filter((c) => c.error).length, "coordinator")} did not say its numbers: the fleet's are the others'.`;
    else note = `The fleet's numbers above add up ${cs.length === 2 ? "both" : "all " + cs.length}.`;
    clear(coordBody, t, note ? h("div", { class: "small muted", style: { padding: "8px 14px 12px" } }, note) : null);
  }

  // ------------------------------------------------------------ who is affected
  function drawWho() {
    if (stats === "forbidden") { clear(whoBody, empty("lock", "Needs an admin key", "Who sends which queries is for admin keys.")); return; }
    if (!stats) { clear(whoBody, h("div", { class: "small muted" }, "Reading the statements of this window…")); return; }
    // Failures as the SLO counts them (the server's: errors and its own
    // deadline), apart from requests the client got wrong and the client's
    // own deadlines (NEW-scenarios-17). A key's failures are split as its
    // statement's are.
    const byTable = new Map(), byKey = new Map();
    let calls = 0, server = 0, client = 0, deadline = 0;
    const add = (m, k, c, e) => { const a = m.get(k) || { calls: 0, errors: 0 }; a.calls += c; a.errors += e; m.set(k, a); };
    for (const st of stats) {
      const c = st.calls || 0;
      const o = st.outcomes || null;
      const sv = o ? (o.error || 0) + (o.timeout || 0) : st.errors || 0;
      const ce = o ? o.client_error || 0 : 0, cd = o ? o.client_deadline || 0 : 0;
      const all = o ? Object.entries(o).filter(([k]) => k !== "ok" && k !== "partial").reduce((a, [, n]) => a + n, 0) : st.errors || 0;
      calls += c; server += sv; client += ce; deadline += cd;
      const tb = tableOf(st.sql || st.example || st.fingerprint || "");
      if (tb) add(byTable, tb, c, sv);
      const share = all ? sv / all : 0;
      for (const k of st.keys || []) add(byKey, k.key, k.calls || 0, (k.errors || 0) * share);
    }
    const failing = server > 0;
    // R2-shell-monitor-3: the statement stats are this coordinator's.
    const through = (state.fleet?.coordinators?.length || 0) > 1 ? ", through this coordinator only" : "";
    whoSub.textContent = failing ? `share of the server's failures, by table${statsKeys ? " and by API key" : ""}, this window${through}` : `share of the queries by table${statsKeys ? " and by API key" : ""}, this window${through}: none failed on the server's side${through ? " here" : ""}`;
    // A bar is the share of the window's server failures (or, with none, of its queries).
    const bars = (m, kind) => [...m.entries()].map(([k, a]) => ({ k, ...a, share: failing ? a.errors / server : a.calls / Math.max(1, calls) }))
      .sort((a, b) => failing ? (b.errors - a.errors) : (b.calls - a.calls)).slice(0, kind === "key" ? 4 : 3)
      .map((a) => h("a", { class: "home-who " + (failing && a.errors / Math.max(1, a.calls) > sloErr() ? "bad" : ""), href: kind === "key" ? link("queries", { key: a.k, tab: "statements" }) : link(byIdTables(), { table: a.k }), title: failing ? `${fmt.pct(a.share, 0)} of the server's failures: ${fmt.n(a.errors)} of its ${fmt.n(a.calls)} queries failed (${pctTxt(a.errors / Math.max(1, a.calls))})` : `${fmt.n(a.calls)} queries, ${fmt.pct(a.share, 0)} of all` },
        h("span", { class: "nm " + (kind === "key" ? "mono" : "") }, a.k), h("span", { class: "home-meter " + (failing ? "crit" : "") }, h("i", { style: { width: Math.max(1, a.share * 100) + "%" } })), h("span", { class: "num small" }, fmt.pct(a.share, a.share < 0.1 ? 1 : 0))));
    const theirs = [client ? `${fmt.n(client)} refused as the client's mistake (4xx)` : null, deadline ? `${fmt.n(deadline)} ran past the client's own deadline` : null].filter(Boolean);
    clear(whoBody,
      byTable.size ? h("div", { class: "stack", style: { gap: "6px" } }, bars(byTable, "table")) : null,
      statsKeys ? h("div", { class: "stack", style: { gap: "6px", marginTop: "10px" } }, bars(byKey, "key")) : h("div", { class: "small muted", style: { marginTop: "10px" } }, "This server does not record which API key sent a query (an older server)."),
      h("div", { class: "small muted", style: { marginTop: "8px" } }, calls ? `${fmt.n(server)} of ${fmt.n(calls)} queries failed on the server's side in ${timeRange().label.replace("last", "the last")}${theirs.length ? `; not counted: ${theirs.join(", ")}` : ""}${stats.length && !statsKeys && !stats[0].prev ? " (an older server counts since it started)" : ""}.` : "No queries in this window."));
  }
  const byIdTables = () => "tables";
  const tableOf = (sql) => { const m = /\bFROM\s+("?)([A-Za-z_][\w.]*)\1/i.exec(sql) || /\bINTO\s+([A-Za-z_][\w.]*)/i.exec(sql); return m ? (m[2] || m[1]) : null; };

  // ------------------------------------------------------------ ready for production (F3, F7, F9, F10)
  const readyBody = h("div");
  const readyTitle = h("h3", {}, "Ready for production");
  const readySub = h("span", { class: "sub" });
  page.append(h("section", { class: "card home-ready" }, h("div", { class: "hd" }, readyTitle, readySub), h("div", { class: "bd" }, readyBody)));
  let auditFile, auditKnown = false;
  async function loadAudit() {
    if (!can("admin", "GET /v1/admin/audit")) { auditKnown = false; drawReady(); return; }
    try { const j = await api("/v1/admin/audit?limit=1", { timeout: 4000 }); auditFile = j.file || null; auditKnown = true; } catch { auditKnown = false; }
    drawReady();
  }
  function drawReady() {
    const w = state.whoami, a = state.alerts, env = environment();
    const rules = (a?.rules || []).filter((r) => r.enabled !== false);
    // CCW-13: a rule shipped with the server has its runbook compiled in
    // (runbook_docs); `webhook` is true when there are routes, not one more.
    const told = rules.filter((r) => (r.owner && r.runbook) || r.runbook_docs).length;
    const routes = (a?.routes || []).length || (a?.webhook ? 1 : 0);
    const shards = state.cluster?.shards || [];
    const single = !isCoordinator();
    const items = [
      { ok: env != null, title: env != null ? `Environment named: ${env}` : "No environment named", fix: "Start the server with --environment production (or staging): the command center then asks before writes and drops.", href: "#/system" },
      { ok: w ? !w.auth?.open : null, title: w?.auth?.open ? "Open: anyone who reaches it can read and change everything" : "Closed: every call needs a key", fix: "Start it with --token, then give each person, app and agent its own key, limited to its tables.", href: "#/access?tab=keys" },
      { ok: routes > 0, title: routes ? `Alerts go somewhere: ${fmt.plural(routes, "route")}` : "Alerts go nowhere: no route (Slack, PagerDuty, a webhook)", fix: "Add a route in Alerts, so that a firing alert reaches a person.", href: "#/alerts?routes=1" },
      { ok: rules.length ? told === rules.length : null, title: rules.length ? `${told} of ${rules.length} alert rules say who owns them and what to do` : "No alert rules", fix: "Give each rule an owner and a runbook URL: the person paged knows it is theirs and where to start.", href: "#/alerts" },
      { ok: auditKnown ? !!auditFile : null, title: !auditKnown ? "Audit log: needs an admin key to tell" : auditFile ? `Audit log kept on disk (${auditFile})` : "Audit log kept in memory only: a restart loses who changed what", fix: "Start the server with --audit-log FILE (the operator: spec.coordinatorStorage, a volume for each coordinator's audit log, events and alerts).", href: "#/access" },
      single ? { ok: null, title: "One server: its disk holds the only copy of the data", fix: "Run a cluster with two or more copies a shard (or back the file up) before the data matters.", href: "#/cluster" }
        : { ok: shards.length ? shards.every((sh) => (sh.replicas || []).length >= 2) : null, title: shards.every((sh) => (sh.replicas || []).length >= 2) ? "Every shard has two copies or more" : (() => { const n = shards.filter((sh) => (sh.replicas || []).length < 2).length; return `${fmt.plural(n, "shard")} ${n === 1 ? "has" : "have"} one copy`; })(), fix: "Add a replica to each shard: a lost disk then loses nothing.", href: "#/cluster" },
    ];
    // LIV-6: copies of a shard on one node, while the cluster has others.
    if (!single) {
      const nodeOf = (rp) => rp.detail?.place?.node || null;
      const all = shards.flatMap((sh) => sh.replicas || []), nodes = new Set(all.map(nodeOf).filter(Boolean));
      const together = shards.filter((sh) => (sh.replicas || []).length >= 2 && (sh.replicas || []).every((rp) => nodeOf(rp) && nodeOf(rp) === nodeOf(sh.replicas[0])));
      if (nodes.size > 1 || together.length) items.push({ ok: nodes.size > 1 ? !together.length : null, title: !together.length ? "Every shard's copies run on more than one node" : `${fmt.plural(together.length, "shard")} with every copy on one node (${together.slice(0, 3).map((sh) => `shard ${sh.shard ?? ""}: ${nodeOf(sh.replicas[0])}`).join(", ")}): losing that node stops ${together.length === 1 ? "it" : "them"}`, fix: "Restart one of its replicas so the scheduler places it on another node, or add a node.", href: "#/cluster" });
    }
    const bad = items.filter((x) => x.ok === false).length;
    readyTitle.textContent = env != null && /^prod/i.test(env) ? "Ready for production" : "Before production";
    readySub.textContent = bad ? `${bad} of ${items.length} not set` : "all set that can be told from here";
    clear(readyBody, h("ul", { class: "ready-list" }, items.map((x) => h("li", { class: x.ok === true ? "ok" : x.ok === false ? "no" : "unk" },
      icon(x.ok === true ? "okc" : x.ok === false ? "warn" : "info"),
      h("div", { class: "grow" }, h("div", {}, x.title), x.ok === true ? null : h("div", { class: "small muted" }, x.fix)),
      x.ok === true ? null : h("a", { class: "btn ghost sm", href: x.href }, "Fix", icon("right"))))));
  }
  async function drawStart() {
    let ts = [];
    try { ts = await catalogTables(); } catch { return; }
    clear(startNudge, ts.some((t) => isMine(t.table)) ? null : h("div", { class: "banner home-start" }, icon("rocket"), h("div", { class: "grow" }, h("b", {}, "None of your data here yet."), h("span", { class: "dim" }, ` ${ts.length ? `The ${fmt.plural(ts.length, "table")} here belong to apps. ` : ""}Import a file or load sample rows, then search it.`)), h("a", { class: "btn primary sm", href: "#/start" }, "Start here", icon("right"))));
  }

  // ------------------------------------------------------------ wiring
  function drawFleet() { drawHead(); drawKpis(); drawBanner(); drawProblems(); drawCoords(); }
  function drawCluster() { drawHead(); drawWhere(); drawCapacity(); drawKpis(); drawBanner(); }
  drawFleet(); drawCluster(); drawCharts(); drawEvents(); drawWho(); drawReady(); drawStart();
  const offs = [
    bus.on("alerts", drawReady), bus.on("whoami", () => { drawReady(); loadAudit(); }), bus.on("cluster", drawReady), every(60000, loadAudit),
    bus.on("fleet", drawFleet), bus.on("cluster", drawCluster), bus.on("alerts", drawBanner),
    bus.on("points", () => { drawCharts(); drawKpis(); }), bus.on("range", () => { drawCharts(); drawHead(); loadEvents(); loadStats(); }),
    bus.on("fresh", () => { drawHead(); drawKpis(); drawWhere(); drawProblems(); chartsCard.classList.toggle("stale", !fresh("series").ok && !fresh("series").loading); }),
    bus.on("answering", () => { drawBanner(); drawHead(); }), bus.on("theme", drawCharts), bus.on("narrow", () => { drawWhere(); drawCharts(); }),
    every(15000, loadEvents), every(30000, loadStats), every(30000, loadTix),
  ];
  return () => offs.forEach((f) => f());
}

// ---------------------------------------------------------------- events, shared with the timeline
/** The view an event opens, at its time (a fixed window around it, which opens paused). */
export function eventLink(e) {
  const t = e.ts_ms, from = t - 15 * 60e3, to = Math.min(Date.now(), (e.until_ms || t) + 15 * 60e3);
  const w = e.where || {};
  const at = { from, to };
  const k = e.kind || "";
  if (k.startsWith("alert")) return link("alert/" + encodeURIComponent(w.rule || e.detail?.rule || ""), at);
  if (k === "rollout" || k === "upgrade") return link("rollout", at);
  if (k === "autoscale") return link("autoscale", at);
  if (k === "change") return link("access", { tab: "changes", q: e.title, ...at });
  if (k === "log_level") return logsLink({ server: w.copy || w.node || null }, at);
  if (k === "contain") return link("queries", { tab: "running", ...at });
  if (k === "note") return link("timeline", { ...at, event: e.id });
  if (w.copy || w.shard != null) return link("cluster", { shard: w.shard, copy: w.copy, ...at });
  return link("timeline", { ...at, event: e.id });
}
export const KINDS = {
  copy_down: "copy", copy_up: "copy", copy_added: "copy", copy_removed: "copy", primary_changed: "primary", majority_lost: "shard", majority_back: "shard",
  shard_added: "shard", shard_removed: "shard", moves_started: "moves", moves_finished: "moves", alert_fired: "alert", alert_resolved: "alert", alert_acked: "on-call",
  alert_silenced: "on-call", rollout: "rollout", autoscale: "autoscale", upgrade: "rollout", change: "change", note: "note", log_level: "config", contain: "contain",
};
export const kindName = (k) => KINDS[k] || (k || "event").replace(/_/g, " ");
export const kindTone = (e) => e.severity === "critical" ? "crit" : e.severity === "warning" ? "warn" : e.kind === "note" ? "note" : ["change", "rollout", "upgrade", "autoscale", "log_level"].includes(e.kind) ? "accent" : "";

function msParts(v) {
  if (v == null) return ["–"];
  const t = fmt.ms(v); const i = t.lastIndexOf(" ");
  return i < 0 ? [t] : [t.slice(0, i), h("small", {}, t.slice(i + 1))];
}
