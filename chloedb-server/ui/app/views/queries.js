// Queries: what runs now, grouped by statement and key, with a safe bulk
// cancel and the contain rules (limit or refuse a statement or a key for
// a while); the statements over the range in the top bar against the
// window before, with cards for error groups, new shapes and those that
// grew most; and the history the log store keeps (7 days), which replaces
// the old Latest and Slow lists. A query opens in a drawer with who sent
// it, its parameters, deadline, outcome, the trace summed up and its
// profile.

import { h, s, icon, clear, fmt, api, card, empty, copy, toast, seg, debounce, failure, table, bus, showTip, hideTip } from "../core.js";
import {
  state, link, when, can, timeRange, poll, paused, isCoord, copyName, OUTCOME_ORDER, outcomeOf, outcomePill, outcomeBar, split, whoChips, paramsText, isOwn,
  shortFp, statementLink, roughFp, statements, change, attention, statementDetail, ask, objects, msOf, parseJson, hasLogStore, logStore, queryColumns,
  queryDrawer, liveDrawer, waterfall, askCancel, containDialog, containRules, containList, profileLink, rounds, narrow, cols as narrowCols, sheet, isNewStatement,
  canContainNow, canContain, causesAndVictims, livePoll, livePaused,
} from "../workload.js";

import { render as statementPage } from "./statement.js";

export { queryDrawer, waterfall };

export default { id: "queries", title: "Queries", icon: "queries", group: "Operate", keywords: "live running cancel recent latest slow statements history trace waterfall contain limit refuse throttle key client pg_stat_statements", render, onParams };

const TABS = [["live", "Running now"], ["statements", "Statements"], ["history", "History"]];
const OLD = { recent: ["history", { preset: "latest" }], slow: ["history", { preset: "slow" }] };

let reroute = null;
function onParams(r) { reroute?.(r.params); }

function render(page, r) {
  const counts = { live: null, statements: null, history: null };
  const tabs = h("div", { class: "tabs", role: "tablist" });
  const body = h("div");
  const scope = h("p", { class: "wl-head-scope" });
  page.append(
    h("div", { class: "page-head" },
      h("div", {}, h("h1", {}, "Queries"), scope),
      h("div", { class: "tools" }, h("button", { class: "btn sm", title: "Copy a link to this view, with its range and filters", onclick: () => copy(location.href, "Link copied") }, icon("link"), "Copy link"))),
    tabs, body);
  // fleet: Running now, Contain, Statements and History cover every coordinator (R2-workload-1).
  const drawScope = () => {
    const c = state.cluster, n = (c?.shards || []).reduce((a, x) => a + (x.replicas?.length || 0), 0);
    const cs = state.fleet?.coordinators || [];
    const shards = n ? ` and ${cs.length > 1 ? "their" : "its"} ${fmt.plural(c.shards.length, "shard")} (${fmt.plural(n, "copy", "copies")})` : "";
    scope.textContent = !isCoord() ? "This server: what runs now, the statements over the range in the top bar, and the history its log store keeps."
      : cs.length > 1 ? `${fmt.plural(cs.length, "coordinator")}${shards}: Running now, Contain, Statements and the history cover every coordinator that answers; each statement says how many calls went through each.`
      : `This coordinator${shards}, the only one the shards heard from: what runs now, the statements over the range in the top bar, and the history the log stores keep.`;
  };
  drawScope();
  // A tab lays out for its width class, so it is built again when the width crosses 900 or 560 px,
  // and only then: a browser can say a media query changed when it did not (a zoom, a full-page capture).
  const layout = () => `${narrow()}/${typeof matchMedia === "function" && matchMedia("(max-width: 560px)").matches}`;
  let lay = layout();
  const offs = [bus.on("cluster", drawScope), bus.on("health", drawScope), bus.on("narrow", () => { const l = layout(); if (l === lay) return; lay = l; key = ""; drawTabs(); show(new URLSearchParams(location.hash.split("?")[1] || "")); })];
  let stop = null, tab = null, key = "";
  function drawTabs() {
    clear(tabs, TABS.map(([k, v]) => h("button", { role: "tab", "aria-selected": String(k === tab), onclick: () => { location.hash = link("queries", { tab: k }); } }, v, counts[k] != null ? h("span", { class: "n" }, counts[k]) : null)),
      narrow() ? null : [h("span", { class: "sep" }), h("span", { class: "small muted", style: { alignSelf: "center" } }, "Latest and Slow are History with a filter")]);
  }
  const setCount = (k, v) => { counts[k] = v; drawTabs(); };
  function show(params) {
    let t = params.get("tab") || "live";
    if (OLD[t]) { const [nt, extra] = OLD[t]; const p = new URLSearchParams(params); p.set("tab", nt); for (const [a, b] of Object.entries(extra)) p.set(a, b); history.replaceState(null, "", link("queries", Object.fromEntries(p))); t = nt; params = p; }
    if (!TABS.some(([k]) => k === t)) t = "live";
    const k = t + "|" + [...params].filter(([a]) => !["from", "to", "range", "tab"].includes(a)).map((x) => x.join("=")).join("&");
    if (k === key) return;
    key = k; tab = t;
    stop?.();
    drawTabs();
    clear(body);
    // #/statement/<fp> lands here, on the Statements tab, while the router has no view for it.
    if (t === "statements" && params.get("fingerprint")) { stop = statementPage(body, { params, rest: [] }); return; }
    stop = t === "live" ? live(body, params, setCount) : t === "statements" ? statementsTab(body, params, setCount) : historyTab(body, params, setCount);
  }
  reroute = show;
  show(r.params);
  return () => { stop?.(); offs.forEach((f) => f()); reroute = null; };
}

// ---------------------------------------------------------------- running now

function live(body, params, setCount) {
  let groupBy = params.get("group") || "statement", filt = params.get("q") || "", own = false, open = new Set(), rules = undefined, stmts = new Map();
  const info = h("div", { class: "row", style: { gap: "12px", marginBottom: "10px" } });
  const search = h("input", { class: "input sm wl-search", placeholder: "Filter: SQL, key, app, address, request id", value: filt, oninput: debounce(() => { filt = search.value.trim().toLowerCase(); draw(); }, 150) });
  const groupSeg = seg([["statement", "statement"], ["key", "key"], ["none", "none"]], groupBy, (v) => { groupBy = v; draw(); }, { title: "Group the running queries" });
  const ownChip = h("button", { class: "chip", "aria-pressed": "false", title: "The command center's own queries (its polling, Query path and Profiler runs) are hidden" });
  ownChip.onclick = () => { own = !own; ownChip.setAttribute("aria-pressed", String(own)); draw(); };
  const tbody = h("tbody"), thead = h("thead");
  const tbl = h("div", { class: "tbl-wrap" }, h("table", { class: "t wl-live" }, thead, tbody));
  let nar = null, span = 7;
  /** The columns: on a phone the query, how long it has run and a cancel; the rest is in its sheet. */
  function head() {
    if (nar === narrow()) return;
    nar = narrow(); span = nar ? 3 : 7;
    clear(thead, h("tr", {}, nar ? [h("th", {}, "query"), h("th", {}, "running for"), h("th", {})] : [h("th", {}, "query"), h("th", {}, "via"), h("th", {}, "running for"), h("th", {}, "key"), h("th", {}, "params"), h("th", {}, "waiting on"), h("th", {})]));
  }
  const rulesBox = h("div");
  const containBtn = can("admin", "POST /v1/admin/contain") ? h("button", { class: "btn sm", onclick: () => containDialog({ onDone: loadRules }) }, icon("shield"), "Contain a key…") : null;
  body.append(
    h("div", { class: "row", style: { gap: "10px", marginBottom: "10px" } }, info, h("span", { class: "sep" }), h("span", { class: "small dim" }, "Group by"), groupSeg, search, ownChip),
    card({ body: tbl, flush: true }),
    h("div", { class: "small muted", style: { margin: "10px 0 14px" } }, "A cancel answers the client 499 and is written to the audit log with your name and reason. Cancelling on a coordinator stops its calls to the shards too."),
    card({ title: "Contain rules", sub: "statements and keys limited or refused for a while", tools: containBtn, body: rulesBox }));
  let all = [], at = 0, err = null, nodes = null, missing = [];
  const shardCount = () => state.cluster?.shards?.length || 0;

  /**
   * The shards a running query waits on, from its trace's rounds: of its
   * last step's calls over all its rounds (a retry is a round of its own),
   * a shard that answered is not waited on, one whose calls failed is being
   * tried again, one with no call ended is pending (R2-scenarios-5).
   * {shards, retrying, failed}, or null when every shard answered.
   */
  function waits(rs) {
    const last = rs[rs.length - 1], n = shardCount();
    if (!last) return null;
    const calls = rs.filter((r) => r.name === last.name).flatMap((r) => r.calls);
    const answered = new Set(calls.filter((c) => c.outcome === "ok" || c.outcome === "outrun").map((c) => c.shard));
    const retrying = [...new Set(calls.filter((c) => !answered.has(c.shard)).map((c) => c.shard))];
    if (retrying.length) return { shards: retrying, retrying: true, failed: calls.filter((c) => retrying.includes(c.shard)).length };
    if (n && answered.size < n) return { shards: (state.cluster?.shards || []).map((x) => x.shard).filter((i) => !answered.has(i)), retrying: false, failed: 0 };
    return null;
  }
  function waitingOn(q) {
    const rs = rounds(q.trace || []);
    if (!rs.length) return q.trace?.length ? h("span", { class: "muted" }, isCoord() ? "planning, or the coordinator" : "running here") : h("span", { class: "muted" }, "–");
    const last = rs[rs.length - 1], n = shardCount();
    const w = waits(rs);
    const names = (xs) => xs.length <= 3 ? xs.map((i) => `shard ${i}`).join(", ") : `${xs.length} of ${n || "the"} shards`;
    if (w?.retrying) return h("span", { class: "small", title: `${fmt.plural(w.failed, "call")} to ${names(w.shards)} failed; the coordinator tries again` }, `${last.name}: `, h("b", {}, names(w.shards)), ` (trying again after ${fmt.plural(w.failed, "failed call")})`);
    if (w) return h("span", { class: "small" }, `${last.name}: `, w.shards.length ? names(w.shards) : "all shards");
    return h("span", { class: "small muted", title: `Every shard answered its ${last.name} call: the coordinator gathers the answer` }, isCoord() ? "the coordinator (every shard answered)" : "–");
  }
  const deadline = (q) => q.deadline_ms ?? state.system?.limits?.timeout_ms ?? null;
  function qRow(q) {
    const lim = deadline(q), f = lim ? Math.min(1, q.elapsed_ms / lim) : null;
    const ran = h("div", { style: { minWidth: nar ? "0" : "150px" } }, h("div", { class: "row nw between small", title: lim ? `of ${q.deadline_ms != null ? "its" : "the server's"} ${fmt.ms(lim)} deadline` : null }, h("b", { class: "nowrap" }, fmt.ms(q.elapsed_ms)), lim && !nar ? h("span", { class: "muted" }, `of ${q.deadline_ms != null ? "its" : "the server's"} ${fmt.ms(lim)} deadline`) : null),
      f != null ? h("div", { class: "meter", style: { marginTop: "3px" } }, h("i", { class: f > 0.8 ? "bad" : f > 0.5 ? "warn" : "", style: { width: f * 100 + "%" } })) : null);
    const act = q.cancelled ? h("span", { class: "pill info" }, "cancelling") : can("admin", "DELETE /v1/queries/:id") ? h("button", { class: "btn sm", onclick: () => cancelOne(q) }, nar ? "Cancel…" : "Cancel query…") : null;
    if (nar) {
      const tr = h("tr", { class: "click", tabindex: 0 },
        h("td", { style: { paddingLeft: "12px" } }, h("div", { class: "mono small" }, q.node ? `${q.node}:${q.local_id ?? q.id}` : q.id), h("div", { class: "small muted ellipsis" }, [q.key, q.app].filter(Boolean).join(" · ") || (groupBy === "none" ? q.sql : ""))),
        h("td", {}, ran), h("td", { class: "num" }, act));
      tr.onclick = (e) => { if (!e.target.closest("button,a")) liveDrawer(q, { onCancel: cancelOne }); };
      return tr;
    }
    const tr = h("tr", { class: "click", tabindex: 0 },
      h("td", { class: "num mono small", style: { paddingLeft: "28px" } }, q.node ? `${q.node}:${q.local_id ?? q.id}` : q.id),
      h("td", {}, q.node ? h("span", { class: "badge" }, q.node) : q.shard ? h("span", { class: "badge", title: "a shard's part of a coordinator's query" }, "coordinator") : h("span", { class: "muted small" }, "client")),
      h("td", {}, ran),
      h("td", { class: "mono small" }, q.key || h("span", { class: "muted" }, "–")),
      h("td", { class: "mono small ellipsis", style: { maxWidth: "220px" }, title: paramsText(q.params, 2000) }, paramsText(q.params, 60) || h("span", { class: "muted" }, "–")),
      h("td", {}, waitingOn(q)),
      h("td", {}, act));
    tr.onclick = (e) => { if (!e.target.closest("button,a")) liveDrawer(q, { onCancel: cancelOne }); };
    tr.onkeydown = (e) => { if (e.key === "Enter") liveDrawer(q, { onCancel: cancelOne }); };
    return tr;
  }
  const cancelOne = (q) => askCancel({ sql: q.sql, queries: [q], key: q.key, onDone: load });

  function groupsOf(qs) {
    const key = groupBy === "statement" ? (q) => q.fingerprint || roughFp(q.sql) : groupBy === "key" ? (q) => q.key || "(no key)" : null;
    if (!key) return [{ id: "all", qs, label: null }];
    const m = new Map();
    for (const q of qs) { const k = key(q); if (!m.has(k)) m.set(k, []); m.get(k).push(q); }
    return [...m].map(([id, list]) => ({ id, qs: list.sort((a, b) => b.elapsed_ms - a.elapsed_ms) })).sort((a, b) => b.qs.length - a.qs.length || b.qs[0].elapsed_ms - a.qs[0].elapsed_ms);
  }
  function groupHead(g) {
    const qs = g.qs, n = qs.length, oldest = qs[0].elapsed_ms;
    const keys = countKeys(qs), vias = [...new Set(qs.map((q) => q.node).filter(Boolean))];
    const fp = groupBy === "statement" ? g.id : null;
    const st = fp ? stmts.get(fp) : null;
    const isOpen = open.has(g.id);
    const mem = qs.reduce((a, q) => a + (q.memory_bytes || 0), 0);
    const sub = [`oldest ${fmt.ms(oldest)}`,
      keys.length ? keys.slice(0, 3).map(([k, c]) => `${k} ${fmt.pct(c / n, 0)}`).join(" · ") + (keys.length > 3 ? ` · ${keys.length - 3} more keys` : "") : null,
      vias.length ? (vias.length > 3 ? `${vias.length} coordinators` : vias.join(", ")) : null,
      mem ? `${fmt.bytes(mem)} query memory` : null].filter(Boolean).join(" · ");
    const bySlow = fp ? slowCopy(qs) : null;
    const isNew = st && isNewStatement(st, 30 * 60e3);
    const fold = h("button", { class: "btn ghost icon sm", "aria-label": isOpen ? "Fold" : "Show every query", onclick: () => { if (isOpen) open.delete(g.id); else open.add(g.id); draw(); } }, icon(isOpen ? "down" : "right"));
    const what = groupBy === "statement" ? h("span", { class: "mono small " + (nar ? "wl-clamp2" : "ellipsis"), style: nar ? null : { maxWidth: "660px" }, title: qs[0].sql }, qs[0].sql) : h("b", { class: "mono small" }, g.id);
    const acts = [
      fp ? h("a", { class: "btn sm", href: statementLink(fp) }, "Statement ›") : null,
      fp && canContainNow() ? h("button", { class: "btn sm", title: "Limit or refuse this statement for a while", onclick: () => containDialog({ fingerprint: fp, sql: qs[0].sql, key: keys.length === 1 ? keys[0][0] : null, keys: keys.map(([k, c]) => ({ key: k, share: c / n })), onDone: loadRules }) }, "Contain…") : null,
      can("admin", "DELETE /v1/queries/:id") && n > 1 ? h("button", { class: "btn sm danger", onclick: () => askCancel({ sql: qs[0].sql, fingerprint: fp, key: groupBy === "key" ? g.id : keys.length === 1 ? keys[0][0] : null, queries: qs, onDone: () => { load(); loadRules(); } }) }, `Cancel all ${fmt.n(n)}…`) : null];
    const ind = nar ? "0" : "34px";
    return h("tr", { class: "wl-ghead" }, h("td", { colspan: span },
      nar ? [h("div", { class: "row nw", style: { gap: "8px" } }, fold, h("span", { class: "pill info" }, `${fmt.n(n)} running`)), h("div", { style: { marginTop: "6px" } }, what)]
        : h("div", { class: "row nw", style: { gap: "10px" } }, fold, h("span", { class: "pill info" }, `${fmt.n(n)} running`), what, h("span", { class: "sep" }), acts),
      h("div", { class: "small muted", style: { margin: `4px 0 0 ${ind}` } }, sub),
      isNew ? h("div", { class: "small", style: { margin: `3px 0 0 ${ind}`, color: "var(--warn)" } }, `New: first seen ${when(st.first_seen_ms)}`) : null,
      bySlow ? h("div", { class: "small wl-bad", style: { margin: `3px 0 0 ${ind}` } }, `${bySlow.n} of ${n} are waiting on shard ${bySlow.shard}`, bySlow.copy ? [", copy ", h("b", { class: "mono" }, copyName(bySlow.copy))] : null) : null,
      nar && acts.some(Boolean) ? h("div", { class: "row", style: { gap: "6px", marginTop: "8px", flexWrap: "wrap" } }, acts) : null));
  }
  function slowCopy(qs) {
    if (qs.length < 3 || !shardCount()) return null;
    const m = new Map();
    for (const q of qs) {
      const w = waits(rounds(q.trace || []));
      for (const sh of w?.shards || []) m.set(sh, (m.get(sh) || 0) + 1);
    }
    const best = [...m].sort((a, b) => b[1] - a[1])[0];
    return best && best[1] >= qs.length * 0.6 && best[1] >= 3 ? { shard: best[0], n: best[1] } : null;
  }

  function draw() {
    head();
    const qs = all.filter((q) => (own || !isOwn(q)) && (!filt || [q.sql, q.key, q.app, q.addr, q.request_id, q.node].some((v) => v && String(v).toLowerCase().includes(filt))));
    const hidden = all.filter((q) => !own && isOwn(q)).length;
    ownChip.textContent = `the command center's own${hidden ? ` (${hidden} hidden)` : ""}`;
    setCount("live", qs.length);
    const oldest = qs.reduce((a, q) => Math.max(a, q.elapsed_ms), 0);
    clear(info, h("span", { class: "pill " + (qs.length ? "info" : "ok") }, `${fmt.n(qs.length)} running`),
      h("span", { class: "small muted", title: state.system ? `This server takes ${state.system.limits?.max_concurrent ?? "?"} queries at once; its timeout is ${fmt.ms(state.system.limits?.timeout_ms)} unless a query asks for less` : "" }, [nodes ? (missing.length ? `on ${nodes - missing.length} of ${fmt.plural(nodes, "coordinator")}: ${missing.map((m) => m.name).join(", ")} did not answer` : `on ${nodes > 1 ? "all " : ""}${fmt.plural(nodes, "coordinator")}`) : isCoord() ? "on this coordinator" : "on this server",
        qs.length ? `oldest ${fmt.ms(oldest)}` : null,
        livePaused() ? "paused" : at ? `refreshed ${fmt.ago(at)}${state.from != null ? ", live: a fixed window does not apply to what runs now" : ""}` : null].filter(Boolean).join(" · ")));
    if (err) { clear(tbody, h("tr", {}, h("td", { colspan: span }, failure(err, "The running queries")))); return; }
    if (!qs.length) { clear(tbody, h("tr", {}, h("td", { colspan: span }, empty("okc", "Nothing running", filt ? "No running query matches the filter." : "A query shows here while it runs: who sent it, with what, and how long it has run against its deadline.")))); return; }
    const rows = [];
    for (const g of groupsOf(qs)) {
      if (groupBy !== "none") rows.push(groupHead(g));
      const show = groupBy === "none" || open.has(g.id) ? g.qs.slice(0, 500) : g.qs.slice(0, 3);
      rows.push(...show.map(qRow));
      if (g.qs.length > show.length) rows.push(h("tr", {}, h("td", { colspan: span, class: "small muted", style: { paddingLeft: nar ? "12px" : "28px" } }, `${fmt.n(g.qs.length - show.length)} more of this ${groupBy === "key" ? "key" : "statement"} · `, h("a", { href: "#", class: "wl-link", onclick: (e) => { e.preventDefault(); open.add(g.id); draw(); } }, "show all"))));
    }
    clear(tbody, rows);
  }
  async function load() {
    try {
      let j;
      try { j = await api("/v1/queries?group=shape"); } catch (e) { if (e.status === 400) j = await api("/v1/queries"); else throw e; }
      all = j.queries || [];
      nodes = j.nodes ? (Array.isArray(j.nodes) ? j.nodes.length : j.nodes) : null;
      missing = Array.isArray(j.nodes) ? j.nodes.filter((x) => x.ok === false) : [];
      at = Date.now(); err = null;
    } catch (e) { err = e; }
    draw();
  }
  async function loadStmts() {
    try { const st = await statements({ from: Date.now() - 3600e3, to: Date.now() }); stmts = new Map(st.statements.map((x) => [x.fingerprint, x])); } catch { /* new shapes are not marked */ }
  }
  async function loadRules() {
    try { rules = await containRules(); } catch (e) { clear(rulesBox, failure(e, "The contain rules")); return; }
    if (containBtn) containBtn.style.display = rules == null ? "none" : "";
    // fleet: a rule holds on every coordinator; say where one is missing or was not asked.
    const cs = state.fleet?.coordinators || [];
    const gaps = (rules || []).filter((r) => r.missing?.length || r.unreachable?.length);
    const where = rules?.length && cs.length > 1 ? h("div", { class: gaps.length ? "notice warn small" : "small muted", style: { marginBottom: "8px" } },
      gaps.length ? gaps.slice(0, 3).map((r) => `Rule ${r.id} holds on ${(r.on || []).join(", ")}${r.missing?.length ? `; missing on ${r.missing.join(", ")}` : ""}${r.unreachable?.length ? `; ${r.unreachable.join(", ")} did not answer` : ""}.`).join(" ")
        : `Each rule holds on all ${cs.length} coordinators: a statement contained on one is refused on every one.`) : null;
    clear(rulesBox, where, containList(rules, { onChange: loadRules }));
  }
  const stops = [livePoll(2000, load), poll(30000, loadStmts), poll(10000, loadRules), ticker(draw), bus.on("narrow", draw)];
  return () => stops.forEach((f) => f());
}

/** Redraws the "refreshed N s ago" once a second. */
function ticker(f) { const t = setInterval(() => { if (!document.hidden) f(); }, 1000); return () => clearInterval(t); }

function countKeys(qs) { const m = new Map(); for (const q of qs) if (q.key) m.set(q.key, (m.get(q.key) || 0) + 1); return [...m].sort((a, b) => b[1] - a[1]); }

// ---------------------------------------------------------------- statements

function statementsTab(body, params, setCount) {
  let only = params.get("changed") === "1", own = false, filt = params.get("q") || "", data = null, err = null, rules = null;
  /** The contain rules that hold a statement now: by its shape, or a key that sends it. */
  const heldBy = (x) => (rules || []).filter((r) => (r.match?.fingerprint || r.match?.key) && (!r.match.fingerprint || r.match.fingerprint === x.fingerprint) && (!r.match.key || (x.keys || []).some((k) => k.key === r.match.key)));
  const head = h("div", { class: "row", style: { gap: "10px", marginBottom: "6px" } });
  const summary = h("div", { class: "small muted", style: { marginBottom: "10px" } });
  const cards = h("div", { class: "grid g3 wl-cards", style: { marginBottom: "14px" } });
  const note = h("div");
  const search = h("input", { class: "input sm wl-search", placeholder: "Filter: SQL or key", value: filt, oninput: debounce(() => { filt = search.value.trim().toLowerCase(); draw(); }, 150) });
  const onlyChip = h("button", { class: "chip", "aria-pressed": String(only), title: "Only the statements that fail, are new, or got slower", onclick: () => { only = !only; onlyChip.setAttribute("aria-pressed", String(only)); draw(); } }, "needs a look only");
  const ownChip = h("button", { class: "chip", "aria-pressed": "false", title: "The command center's own statements (its polling of the log store, Query path and Profiler runs)", onclick: () => { own = !own; ownChip.setAttribute("aria-pressed", String(own)); draw(); } }, "the command center's own");
  const windowMs = () => { const r = timeRange(); return (r.to ?? Date.now()) - r.from; };
  // A rate is over the part of the range the server holds.
  const perS = (n) => n / Math.max(1, (data?.span_ms ?? windowMs()) / 1000);
  const nar = narrow();
  const tbl = table(narrowCols([
    { key: "sql", label: "statement", cls: "sql", sort: (x) => x.sql, render: (x) => { const a = x._att; const held = heldBy(x);
      const tag = [a ? h("span", { class: "pill " + a.tone, style: { padding: "0 7px" } }, a.tag) : null,
        held.length ? h("span", { class: "pill info", style: { padding: "0 7px" }, title: held.map((r) => `rule ${r.id}: ${r.action === "refuse" ? "refused" : `at most ${r.concurrency} at once`}${r.match?.key ? " from key " + r.match.key : ""}, ends ${fmt.ago(r.expires_ms)}: “${r.reason}”`).join("\n") }, held.some((r) => r.action === "refuse") ? "contained: refused" : "contained: limited") : null].filter(Boolean);
      return nar ? h("div", {}, tag, h("div", { class: "mono small wl-clamp2", title: x.sql, style: { marginTop: tag.length ? "3px" : 0 } }, x.sql)) : h("span", { class: "row nw", style: { gap: "6px" } }, tag, h("span", { class: "ellipsis", title: x.sql }, x.sql)); } },
    { key: "keys", label: "keys", wide: true, sort: (x) => x.keys?.[0]?.key || "", render: (x) => x.keys?.length ? h("span", { class: "small nowrap", title: x.keys.map((k) => `${k.key}: ${fmt.n(k.calls)} calls`).join("\n") }, h("span", { class: "mono" }, x.keys[0].key), x.keys.length > 1 ? h("span", { class: "muted" }, ` +${x.keys.length - 1}`) : null) : h("span", { class: "muted" }, "–") },
    { key: "first_seen_ms", label: "since", num: true, wide: true, render: (x) => x.first_seen_ms ? h("span", { class: "nowrap small", title: fmt.datetime(x.first_seen_ms) }, when(x.first_seen_ms)) : h("span", { class: "muted" }, "–") },
    { key: "calls", label: "calls/s", num: true, render: (x) => h("span", { title: `${fmt.n(x.calls)} calls in the window` }, fmt.si(perS(x.calls))) },
    { key: "p99_ms", label: "p99", num: true, render: (x) => h("b", { class: "nowrap" }, fmt.ms(x.p99_ms)) },
    { key: "dp99", label: "vs before", num: true, wide: true, sort: (x) => change(x)?.p99X ?? 0, render: (x) => { const c = change(x); if (!c || c.p99X == null) return h("span", { class: "muted", title: data?.prevHeld ? "" : "The server does not hold the window before" }, x.prev === null && data?.windowed && isNewStatement(x, windowMs()) ? "new" : "–"); return h("span", { class: c.p99X >= 2 ? "wl-bad" : "dim", title: `p99 ${fmt.ms(x.prev.p99_ms)} in the window before · calls ×${c.callsX.toFixed(1)}` }, `×${c.p99X.toFixed(1)}`); } },
    { key: "rows_per_call", label: "rows/call", num: true, wide: true, render: (x) => fmt.si(x.rows_per_call) },
    { key: "failed", label: "failed", num: true, wide: true, sort: (x) => split(x.outcomes, x.calls).failed, render: (x) => { const sp = split(x.outcomes, x.calls); return h("span", { class: "row nw", style: { gap: "8px", justifyContent: "flex-end" } }, sp.failed ? h("span", { class: sp.server / Math.max(1, sp.calls) > 0.01 ? "wl-bad" : "dim", title: [sp.server ? `${fmt.pct(sp.server / Math.max(1, sp.calls), 1)} failed on the server` : null, sp.refused ? `${fmt.pct(sp.refused / Math.max(1, sp.calls), 1)} refused (429): no slot, no memory, or a contain rule` : null, sp.client ? `${fmt.pct(sp.client / Math.max(1, sp.calls), 1)} client errors (400)` : null, sp.deadlines ? `${fmt.pct(sp.deadlines / Math.max(1, sp.calls), 1)} past the client's deadline` : null].filter(Boolean).join("\n") }, fmt.pct(sp.failed / Math.max(1, sp.calls), 1)) : h("span", { class: "muted" }, "0"), outcomeBar(x.outcomes, 60)); } },
    { key: "share", label: "share of time", num: true, wide: true, render: (x) => h("div", { class: "row nw", style: { gap: "6px", justifyContent: "flex-end" } }, h("div", { class: "barbox", style: { width: "56px" } }, h("div", { class: "bar", style: { width: Math.min(100, x.share * 100) + "%" } })), h("span", { class: "small dim num" }, fmt.pct(x.share, 0))) },
  ]), { sortKey: null, onRow: (x) => { location.hash = statementLink(x.fingerprint); }, empty: empty("queries", "No statements in this range", "Queries are grouped by shape: the same SQL with other literals.") });
  tbl.classList.add("wl-stmts");
  // Until the first answer, say it is reading: an empty table would say there is nothing.
  const tblCard = card({ body: tbl, flush: true, cls: "wl-nums" });
  tblCard.style.display = "none";
  clear(note, h("div", { class: "small muted", style: { margin: "6px 0 12px" } }, "Reading the statements of the range…"));
  body.append(h("div", { class: "row", style: { gap: "10px", marginBottom: "6px" } }, head, h("span", { class: "sep" }), onlyChip, ownChip, search), summary, note, cards, tblCard);

  function draw() {
    const r = timeRange();
    const mins = Math.round(windowMs() / 60e3);
    clear(head, h("span", { class: "pill info" }, `${r.label || "the range"} · ${fmt.time(r.from, false)} → ${r.to && !r.live ? fmt.time(r.to, false) : "now"}`),
      data?.windowed && data.partial ? h("span", { class: "small muted", title: `The server keeps statements from ${fmt.datetime(data.held_from_ms)}: rates are over that part of the range` }, `held from ${when(data.held_from_ms)} on`) : null,
      // R2-workload-1: a coordinator that did not answer leaves its statements out.
      data?.coordinators?.some((c) => !c.ok) ? h("span", { class: "small", style: { color: "var(--warn)" } }, `${data.coordinators.filter((c) => !c.ok).map((c) => c.name || c.url).join(", ")} did not answer: ${data.coordinators.filter((c) => !c.ok).length === 1 ? "its" : "their"} statements are not here`) : null,
      data?.windowed ? h("span", { class: "small muted" }, data.prevHeld ? `compared with the ${mins >= 120 ? fmt.plural(Math.round(mins / 60), "hour") : mins + " min"} before` : "no window before to compare with: the server does not hold it") : null);
    if (err) { clear(note, failure(err, "The statements")); clear(cards); tbl.update([]); return; }
    if (!data) return;
    tblCard.style.display = "";
    clear(note, data.windowed ? null : h("div", { class: "banner warn", style: { marginBottom: "12px" } }, icon("warn"), h("div", { class: "grow" }, h("b", {}, "This server counts statements since it started"), h("div", { class: "small" }, `${data.since_ms ? "Since " + fmt.datetime(data.since_ms) + ", " : ""}not over the range in the top bar, and with no window before to compare with. A newer server answers for any range.`))));
    let xs = data.statements.filter((x) => own || !isOwn(x));
    const hidden = data.statements.length - xs.length;
    ownChip.textContent = `the command center's own${hidden && !own ? ` (${hidden} hidden)` : ""}`;
    for (const x of xs) x._att = attention(x, windowMs());
    // NEW-workload-12: the statement's id (the server's, or the short one an older page showed) finds it too.
    if (filt) xs = xs.filter((x) => x.sql.toLowerCase().includes(filt) || x.fingerprint?.toLowerCase().includes(filt) || (x.id && x.id.startsWith(filt)) || shortFp(x.fingerprint) === filt || (x.keys || []).some((k) => k.key?.toLowerCase().includes(filt)));
    const flagged = xs.filter((x) => x._att);
    // Those that need a look first, worst first; then by time spent.
    xs.sort((a, b) => (b._att?.rank || 0) - (a._att?.rank || 0) || b.total_ms - a.total_ms);
    tbl.update(only ? xs.filter((x) => x._att) : xs);
    setCount("statements", fmt.n(xs.length));
    const calls = xs.reduce((a, x) => a + x.calls, 0), prevCalls = xs.reduce((a, x) => a + (x.prev?.calls || 0), 0);
    const sp = xs.reduce((a, x) => { const t = split(x.outcomes, x.calls); a.server += t.server; a.client += t.client; a.dl += t.deadlines; return a; }, { server: 0, client: 0, dl: 0 });
    const prevErr = xs.reduce((a, x) => a + (x.prev?.errors || 0), 0);
    clear(summary, [`${fmt.si(perS(calls))} q/s${data.windowed && prevCalls && data.prevHeld ? ` (was ${fmt.si(prevCalls / Math.max(1, windowMs() / 1000))})` : ""}`,
      `server errors ${fmt.pct(sp.server / Math.max(1, calls), 2)}${data.windowed && prevCalls ? ` (was ${fmt.pct(prevErr / Math.max(1, prevCalls), 2)})` : ""}`,
      sp.client ? `client errors ${fmt.pct(sp.client / Math.max(1, calls), 2)}` : null,
      sp.dl ? `client deadlines ${fmt.pct(sp.dl / Math.max(1, calls), 2)}, counted apart` : null,
      `${fmt.plural(flagged.length, "statement")} need a look`].filter(Boolean).join(" · "));
    drawCards(xs, calls);
  }
  const msgCache = new Map();
  function drawCards(xs, calls) {
    const errs = xs.filter((x) => { const t = split(x.outcomes, x.calls); return t.server + t.client > 0; }).sort((a, b) => { const A = split(a.outcomes, a.calls), B = split(b.outcomes, b.calls); return (B.server + B.client) - (A.server + A.client); });
    const allFailed = errs.reduce((a, x) => { const t = split(x.outcomes, x.calls); return a + t.server + t.client; }, 0);
    const news = xs.filter((x) => isNewStatement(x, windowMs())).sort((a, b) => b.total_ms - a.total_ms);
    // S4-3: causes (new, or more calls) before victims (slower, calls flat).
    const cv = data.windowed ? causesAndVictims(xs, windowMs()) : { causes: [], victims: [], cause: null };
    const grew = [...cv.causes.filter((y) => !y.c.isNew), ...cv.victims].slice(0, 3).map((y) => [y.x, y.c]);
    // NEW-scenarios-14: a server that started after the range began (or
    // less than a minute before) judges no statement new.
    const cantTell = data.windowed && data.since_ms != null && data.since_ms + 60e3 >= (data.from ?? timeRange().from);

    const errCard = h("section", { class: "card wl-card" + (errs.length ? " bad" : "") }, h("div", { class: "hd" }, h("h3", {}, errs.length ? `Errors: ${fmt.plural(Math.min(errs.length, 99), "statement")}` : "Errors"), h("span", { class: "sub" }, "by statement and key")), h("div", { class: "bd" }));
    const eb = errCard.lastChild;
    if (!errs.length) eb.append(h("div", { class: "small muted" }, "No statement failed in this range."));
    else {
      const x = errs[0], t = split(x.outcomes, x.calls), failed = t.server + t.client;
      const others = calls - x.calls, othersFailed = allFailed - failed;
      const k = x.keys?.slice().sort((a, b) => (b.errors || 0) - (a.errors || 0))[0];
      const msg = h("span", { class: "mono small" });
      // NEW-scenarios-20: when this key's calls began to fail (the log
      // store keeps every failed query), not when the statement was first seen.
      const since = h("span", { class: "badge", title: "This key's first failed call of this statement in the range, from the log store" });
      since.style.display = "none";
      failingSince(x.fingerprint, k?.key).then((t) => {
        if (!t) return;
        // A first failure in the range's first minute: it failed before the range too.
        const all = t - timeRange().from < 60e3;
        since.textContent = `${k ? "its calls failing" : "failing"} ${all ? "all range" : "since " + when(t)}`;
        if (all) since.title = "Its first failed call in the range came in the range's first minute: widen the range to see when it began";
        since.style.display = "";
      }).catch(() => {});
      eb.append(
        h("div", { class: "mono small ellipsis", title: x.sql }, x.sql),
        h("div", { class: "small", style: { margin: "6px 0" } }, h("b", { class: t.server ? "wl-bad" : "" }, `${fmt.si(perS(failed))}/s failing, ${fmt.pct(failed / Math.max(1, x.calls), 0)} of its calls`), " · ", t.client >= t.server ? "client errors (400)" : "server errors", allFailed ? ` · ${fmt.pct(failed / allFailed, 0)} of all failures` : "", " ", msg),
        h("div", { class: "row", style: { gap: "4px" } }, k ? h("span", { class: "badge mono" }, "key " + k.key) : null, since, (x.keys?.length || 0) > 1 ? h("span", { class: "badge", title: x.keys.filter((y) => y !== k).map((y) => `${y.key}: ${fmt.n(y.errors || 0)} failed of ${fmt.n(y.calls)}`).join("\n") }, fmt.plural(x.keys.length - 1, "other key")) : null),
        others > 0 ? h("div", { class: "small muted", style: { marginTop: "6px" } }, `The rest: ${fmt.pct(othersFailed / Math.max(1, others), 1)} of every other statement's calls failed.`) : null,
        h("div", { class: "row", style: { gap: "6px", marginTop: "8px" } },
          h("a", { class: "btn sm", href: statementLink(x.fingerprint) }, "Open it ›"),
          canContainNow() ? h("button", { class: "btn sm", onclick: () => containDialog({ fingerprint: x.fingerprint, sql: x.sql, key: k?.key, keys: x.keys, action: "refuse", onDone: () => {} }) }, k ? `Refuse it from ${k.key}…` : "Refuse it…") : null,
          h("button", { class: "btn sm ghost", title: "A summary to send the team that owns the client", onclick: () => copy(`Statement ${x.id || shortFp(x.fingerprint)} fails: ${fmt.n(failed)} of ${fmt.n(x.calls)} calls (${fmt.pct(failed / Math.max(1, x.calls), 0)}) ${timeRange().label || ""}${k ? `, from API key ${k.key}` : ""}.\n${msg.textContent ? "Error: " + msg.textContent + "\n" : ""}SQL: ${x.sql}\n${location.origin}/${statementLink(x.fingerprint)}`, "Summary copied") }, icon("copy"), "Copy for the app team")),
        errs.length > 1 ? h("div", { class: "small muted", style: { marginTop: "8px" } }, errs.slice(1, 3).map((y) => h("div", { class: "ellipsis" }, h("a", { href: statementLink(y.fingerprint), class: "wl-link" }, `${fmt.n(split(y.outcomes, y.calls).failed)} failed`), " · ", h("span", { class: "mono" }, y.sql)))) : null);
      const m = msgCache.get(x.fingerprint);
      if (m && Date.now() - m.at < 30e3) msg.textContent = m.text;
      else statementDetail(x.fingerprint, timeRange()).then((d) => { const e = d.errors?.[0]; const text = e ? `“${e.message}”` : ""; msgCache.set(x.fingerprint, { at: Date.now(), text }); msg.textContent = text; }).catch(() => {});
    }
    const newCard = card({ title: news.length ? `New: ${fmt.plural(news.length, "shape")}` : "New shapes", sub: !data.windowed ? "needs a newer server" : cantTell ? "can't tell for this range" : "first seen in this range", cls: "wl-card" + (news.length ? " warn" : ""),
      body: !news.length ? h("div", { class: "small muted" }, !data.windowed ? "This server does not say when a statement was first seen." : cantTell ? `Can't tell: this server started at ${when(data.since_ms)}, after the range began or less than a minute before, so it has nothing to compare with. Pick a range that starts after ${when(data.since_ms + 60e3)}.` : "No statement was first seen in this range.") : (() => {
        const x = news[0], k = x.keys?.[0];
        return h("div", {},
          h("div", { class: "mono small ellipsis", title: x.sql }, x.sql),
          h("div", { class: "small", style: { margin: "6px 0" } }, `${fmt.si(perS(x.calls))}/s · `, h("b", {}, `${fmt.si(x.rows_per_call)} rows a call`), ` · p99 ${fmt.ms(x.p99_ms)} · ${fmt.pct(x.share, 0)} of query time · first seen ${when(x.first_seen_ms)}`),
          h("div", { class: "row", style: { gap: "4px" } }, (x.keys || []).slice(0, 3).map((y) => h("span", { class: "badge mono" }, "key " + y.key))),
          h("div", { class: "row", style: { gap: "6px", marginTop: "8px" } },
            h("a", { class: "btn sm", href: statementLink(x.fingerprint) }, "Open it ›"),
            canContainNow() ? h("button", { class: "btn sm", onclick: () => containDialog({ fingerprint: x.fingerprint, sql: x.sql, key: k?.key, keys: x.keys }) }, k ? `Limit ${k.key}…` : "Limit it…") : null,
            h("a", { class: "btn sm ghost", href: profileLink(x.sql, x.sample?.params, x.fingerprint), title: "Open it in the Profiler: it asks before it runs" }, icon("flame"), "Profile one")),
          news.length > 1 ? h("div", { class: "small muted", style: { marginTop: "8px" } }, news.slice(1, 3).map((y) => h("div", { class: "ellipsis" }, h("a", { href: statementLink(y.fingerprint), class: "wl-link" }, when(y.first_seen_ms)), " · ", h("span", { class: "mono" }, y.sql)))) : null);
      })() });
    const victimOf = cv.cause ? (cv.cause.c.isNew ? `the new statement first seen ${when(cv.cause.x.first_seen_ms)}` : `the statement with ×${cv.cause.c.callsX.toFixed(1)} the calls`) : null;
    const grewCard = card({ title: "Grew most", sub: data.windowed ? "by time spent, against the window before · causes first" : "needs a newer server", cls: "wl-card",
      body: !grew.length ? h("div", { class: "small muted" }, data.windowed ? "No statement grew by half or more." : "This server keeps no window before to compare with.") : h("div", { class: "stack", style: { gap: "8px" } }, grew.map(([x, c]) => h("div", {},
        h("a", { class: "mono small ellipsis wl-link", style: { display: "block" }, href: statementLink(x.fingerprint), title: x.sql }, x.sql),
        h("div", { class: "small dim" }, `p99 ${fmt.ms(x.prev.p99_ms)} → ${fmt.ms(x.p99_ms)}, calls ×${c.callsX.toFixed(1)}`,
          c.callsX < 1.5 ? h("b", {}, ": slower, calls flat. Likely a victim", victimOf && cv.cause.x !== x ? [" of ", h("a", { class: "wl-link", href: statementLink(cv.cause.x.fingerprint), title: cv.cause.x.sql }, victimOf)] : ", not a cause") : c.verdict ? h("b", {}, ": " + c.verdict) : null)))) });
    clear(cards, errCard, newCard, grewCard);
  }
  async function load() {
    try { [data] = await Promise.all([statements(timeRange()), canContain()]); err = null; } catch (e) { err = e; }
    rules = canContainNow() ? await containRules().catch(() => null) : null;
    draw();
  }
  const stops = [poll(10000, load), bus.on("range", load)];
  return () => stops.forEach((f) => f());
}

// ---------------------------------------------------------------- history

const PRESETS = [["all", "all"], ["failed", "failed"], ["slow", "slow"], ["latest", "last 15 min"]];

/** When `key`'s calls of a statement began to fail in the range (its first failed call the log store keeps), or null. */
const sinceCache = new Map();
async function failingSince(fp, key) {
  if (!fp || !(await hasLogStore())) return null;
  const r = timeRange(), ck = `${fp}|${key}|${Math.round(r.from / 60e3)}`, hit = sinceCache.get(ck);
  if (hit && Date.now() - hit.at < 30e3) return hit.t;
  const t = await firstFailed(fp, key, r);
  sinceCache.set(ck, { at: Date.now(), t });
  return t;
}
async function firstFailed(fp, key, r) {
  const lit = (v) => `'${String(v).replace(/'/g, "''")}'`, cols = await queryColumns();
  const kc = cols.has("key") ? "key" : cols.has("key_name") ? "key_name" : null;
  const w = [`fingerprint = ${lit(fp)}`, "shard = false", "outcome != 'ok'", `ts >= ${lit(new Date(r.from).toISOString())}`, r.to && !r.live ? `ts < ${lit(new Date(r.to).toISOString())}` : null, key && kc ? `${kc} = ${lit(key)}` : null].filter(Boolean).join(" AND ");
  const j = await ask(`SELECT ts FROM queries WHERE ${w} ORDER BY ts ASC LIMIT 1`);
  return j.rows?.[0] ? msOf(j.rows[0][0]) : null;
}

function historyTab(body, params, setCount) {
  const f = {
    preset: params.get("preset") || "all", q: params.get("q") || "", key: params.get("key") || "", fingerprint: params.get("fingerprint") || "",
    request: params.get("request") || "", min: params.get("min") || "", kind: params.get("kind") || "", parts: false, own: false,
  };
  const presetSeg = seg(PRESETS, f.preset, (v) => { f.preset = v; refresh(); }, { title: "Latest is the last 15 minutes; slow is over the slow threshold" });
  const q = h("input", { class: "input sm", placeholder: "Search the SQL", value: f.q, style: { width: "180px" }, oninput: debounce(() => { f.q = q.value.trim(); refresh(); }, 300) });
  const keyIn = h("input", { class: "input sm mono", placeholder: "key", value: f.key, style: { width: "120px" }, oninput: debounce(() => { f.key = keyIn.value.trim(); refresh(); }, 300) });
  const min = h("input", { class: "input sm num", type: "number", min: 0, placeholder: "ms", value: f.min, style: { width: "74px" }, title: "Only queries that took at least this many milliseconds", oninput: debounce(() => { f.min = min.value; refresh(); }, 300) });
  const parts = h("label", { class: "row nw small dim", style: { gap: "6px", cursor: "pointer" }, title: "A coordinator's query runs a part on each shard, which each shard keeps too" }, h("input", { type: "checkbox", onchange: (e) => { f.parts = e.target.checked; refresh(); } }), "shards' parts");
  const ownBox = h("label", { class: "row nw small dim", style: { gap: "6px", cursor: "pointer" }, title: "The command center's own queries" }, h("input", { type: "checkbox", onchange: (e) => { f.own = e.target.checked; refresh(); } }), "the command center's own");
  const chips = h("div", { class: "row", style: { gap: "6px" } });
  const note = h("div");
  const hist = h("div", { class: "wl-hist" });
  const count = h("span", { class: "small muted" });
  const more = h("div", { class: "row", style: { padding: "8px 12px" } });
  const asSql = h("a", { class: "btn sm ghost", title: "This search as SQL over the log store, to change it" }, icon("sql"), "As SQL");
  let cols = new Set(), store = false, rows = [], total = 0, gen = 0;
  const pick = (k) => (v) => { f[k] = v; if (k === "key") keyIn.value = v; refresh(); };
  const nar = narrow();
  const tbl = table(narrowCols([
    { key: "ts_ms", label: "time", render: (x) => nar ? h("div", {}, h("div", { class: "mono small nowrap", title: fmt.datetime(x.ts_ms) }, when(x.ts_ms)), x.outcome && x.outcome !== "ok" ? h("div", { style: { marginTop: "3px" } }, outcomePill(x.outcome)) : null) : h("span", { class: "mono small nowrap", title: fmt.datetime(x.ts_ms) }, when(x.ts_ms)) },
    { key: "node", label: "server", wide: true, render: (x) => x.node ? h("span", { class: "badge mono" }, x.node) : "–" },
    { key: "outcome", label: "outcome", wide: true, render: (x) => outcomePill(x.outcome) },
    { key: "ms", label: "took", num: true, render: (x) => h("b", { class: "nowrap" }, fmt.ms(x.ms)) },
    { key: "rows", label: "rows", num: true, wide: true, render: (x) => fmt.n(x.rows) },
    { key: "key", label: "key", wide: true, render: (x) => x.key ? h("button", { class: "btn ghost sm mono", title: "Only this key's", onclick: () => pick("key")(x.key) }, x.key) : h("span", { class: "muted" }, "–") },
    { key: "app", label: "app · address", wide: true, render: (x) => x.app || x.addr ? h("span", { class: "small nowrap" }, x.app || "", x.app && x.addr ? h("span", { class: "muted" }, " · ") : null, x.addr ? h("span", { class: "mono muted" }, x.addr) : null) : h("span", { class: "muted" }, "–") },
    { key: "sql", label: "SQL", render: (x) => h("div", { class: "mono small " + (nar ? "wl-clamp2" : "ellipsis"), style: { maxWidth: nar ? "190px" : "520px" }, title: x.error ? `${x.sql}\n\n${x.error}` : x.sql }, x.shard ? h("span", { class: "badge", style: { marginRight: "6px" } }, "shard part") : null, x.sql) },
  ]), { sortKey: null, compact: true, onRow: (x) => queryDrawer(x), empty: empty("queries", "No queries match", "Try a longer range or fewer filters. Fast queries that ended well are kept at most --log-queries a second; every slow or failed one is kept.") });
  body.append(
    h("div", { class: "row", style: { gap: "10px", marginBottom: "8px" } }, presetSeg, q, keyIn, h("span", { class: "row nw small muted", style: { gap: "6px" } }, "at least", min), parts, ownBox, asSql, h("span", { class: "sep" }), count),
    chips, note,
    card({ title: "Queries kept, over time", sub: "by how they ended · the log store keeps every failed and slow query and a sample of the fast ones (--log-queries a second), so failures look more common here than they are", body: hist, attrs: { style: { marginBottom: "12px" } } }),
    card({ title: "Queries", sub: "newest first · click one for who sent it, its trace and profile", body: [tbl, more], flush: true }));

  const lit = (v) => `'${String(v).replace(/'/g, "''")}'`;
  const slowMs = () => state.system?.limits?.slow_ms ?? 200;
  function span() {
    const r = timeRange();
    let from = f.preset === "latest" ? Date.now() - 15 * 60e3 : r.from;
    const to = f.preset === "latest" ? null : r.live ? null : r.to;
    const floor = Date.now() - 7 * 86400e3;
    return { from: Math.max(from, floor), to, clipped: from < floor };
  }
  const has = (c) => cols.has(c);
  // The log store names the key's column key_name; an older draft named it key.
  const keyCol = () => has("key") ? "key" : has("key_name") ? "key_name" : null;
  function where(skip) {
    const t = span();
    return [
      `ts >= ${lit(new Date(t.from).toISOString())}`, t.to ? `ts < ${lit(new Date(t.to).toISOString())}` : null,
      f.preset === "failed" ? "outcome != 'ok'" : null,
      f.preset === "slow" ? `ms >= ${slowMs()}` : null,
      f.q ? `MATCH(sql, ${lit(f.q)})` : null,
      f.request ? `request_id = ${lit(f.request)}` : null,
      f.kind ? `kind = ${lit(f.kind)}` : null,
      f.fingerprint && skip !== "fingerprint" ? `fingerprint = ${lit(f.fingerprint)}` : null,
      f.key && keyCol() ? `${keyCol()} = ${lit(f.key)}` : null,
      f.min !== "" && Number.isFinite(+f.min) ? `ms >= ${+f.min}` : null,
      f.parts ? null : "shard = false",
      !f.own && has("client") ? "(client IS NULL OR client != 'command-center')" : null,
    ].filter(Boolean).join(" AND ");
  }
  const extra = () => [keyCol() === "key_name" ? "key_name AS key" : keyCol(), ...["key_id", "addr", "app", "client", "params", "deadline_ms"].filter(has)].filter(Boolean);
  const rowsSql = (tail = "") => `SELECT ts, node, request_id, kind, outcome, shard, sql, fingerprint, ms, n_rows, error, detail, id${extra().map((c) => ", " + c).join("")} FROM queries WHERE ${where()}${tail} ORDER BY ts DESC, id DESC LIMIT 200`;
  const finished = (r) => {
    const d = parseJson(r.detail) || {};
    const x = { ...d, ts_ms: msOf(r.ts), request_id: r.request_id, kind: r.kind, outcome: r.outcome, shard: r.shard, sql: r.sql, fingerprint: r.fingerprint, ms: r.ms, rows: r.n_rows, error: r.error, node: r.node, _id: r.id, _ts: r.ts };
    for (const c of extra().map((e) => e.replace(/^.* AS /, ""))) if (r[c] != null) x[c] = c === "params" ? parseJson(r[c]) ?? r[c] : r[c];
    return x;
  };
  function drawChips() {
    clear(chips, [["fingerprint", "statement"], ["request", "request"], ["key", "key"], ["kind", "kind"]].filter(([k]) => f[k]).map(([k, l]) => h("button", { class: "chip", "aria-pressed": "true", title: "Remove", onclick: () => pick(k)("") }, h("span", { class: "ellipsis", style: { maxWidth: "520px" } }, `${l} = ${f[k]}`), h("span", { class: "x" }, "×"))));
    chips.style.marginBottom = chips.childElementCount ? "8px" : "0";
  }
  function own(xs) { return f.own || has("client") ? xs : xs.filter((x) => !isOwn(x)); }

  async function refresh() {
    const my = ++gen;
    drawChips();
    if (!store) return ring(my);
    const t = span(), unit = (t.to ?? Date.now()) - t.from > 1440 * 60e3 ? 60 : 1, col = unit === 60 ? "hour" : "minute";
    asSql.href = link("logs", { tab: "sql", sql: rowsSql() });
    clear(note, [t.clipped ? h("div", { class: "notice", style: { marginBottom: "10px" } }, "The log store keeps 7 days: the range is cut to them.") : null,
      f.key && !keyCol() ? h("div", { class: "notice", style: { marginBottom: "10px" } }, "This server's log store does not record keys yet: the key filter is not applied.") : null]);
    try {
      // What was served (every call, not only those kept), when no filter
      // narrows it: the statements of the range.
      const plain = !f.q && !f.key && !f.request && f.min === "" && !f.kind && f.preset !== "slow";
      const [rs, hs, by, st] = await Promise.all([
        ask(rowsSql()),
        ask(`SELECT ${col}, outcome, COUNT(*) FROM queries WHERE ${where()} GROUP BY ${col}, outcome LIMIT 20000`),
        ask(`SELECT outcome, COUNT(*) FROM queries WHERE ${where()} GROUP BY outcome`),
        plain ? statements({ from: t.from, to: t.to }).catch(() => null) : null,
      ]);
      if (my !== gen) return;
      rows = own(objects(rs).map(finished));
      total = by.rows.reduce((a, r) => a + r[1], 0);
      const bad = (rs.nodes || []).filter((n) => n.error);
      if (bad.length) note.append(h("div", { class: "banner warn" }, icon("warn"), h("div", { class: "grow" }, h("b", {}, `${fmt.plural(bad.length, "server")} did not answer, so ${bad.length === 1 ? "its" : "their"} queries are missing here`), bad.map((n) => h("div", { class: "small mono" }, `${n.node || n.url}: ${n.error}`)))));
      histogram(hist, hs.rows, { unit, t });
      tbl.update(rows);
      count.textContent = `${fmt.n(total)} kept · ${by.rows.map(([o, n]) => `${fmt.n(n)} ${outcomeOf(o).label}`).join(" · ")}`;
      count.title = "Rows the log store kept: every failed and slow query, and at most --log-queries fast ones a second";
      // Served: every call of the range, from the statements (this coordinator's).
      const sv = st?.windowed ? servedOf(st.statements.filter((x) => !isOwn(x) && (!f.fingerprint || x.fingerprint === f.fingerprint || x.id === f.fingerprint))) : null;
      if (sv?.calls) note.append(h("div", { class: "small muted", style: { marginBottom: "8px" } }, `Served ${st.partial ? `since ${when(st.held_from_ms)}` : "in the range"}: ${fmt.plural(sv.calls, "query", "queries")}`,
        `, ${fmt.si(sv.server / sv.calls * 100, 2)}% failed on the server`, sv.client ? `, ${fmt.si(sv.client / sv.calls * 100, 2)}% client errors` : "", sv.deadline ? `, ${fmt.si(sv.deadline / sv.calls * 100, 2)}% past the client's deadline` : "",
        `. The rows below are the ${fmt.n(total)} the log store kept${total ? ` (${fmt.pct(total / sv.calls, total / sv.calls < 0.1 ? 1 : 0)} of them)` : ""}.`));
      setCount("history", "7 days");
      drawMore();
    } catch (e) { if (my === gen) { clear(note, failure(e, "The query history")); tbl.update([]); clear(hist); } }
  }
  function drawMore() {
    clear(more, rows.length ? [h("span", { class: "small muted" }, `${fmt.n(rows.length)} of ${fmt.plural(total, "query", "queries")} shown`), rows.length < total ? h("button", { class: "btn sm", style: { marginLeft: "8px" }, onclick: older }, icon("down"), "Older") : null] : null);
  }
  async function older() {
    if (!rows.length) return;
    const my = gen, last = rows[rows.length - 1];
    try {
      const j = await ask(rowsSql(` AND (ts < ${lit(last._ts)} OR (ts = ${lit(last._ts)} AND id < ${last._id}))`));
      if (my !== gen) return;
      const seen = new Set(rows.map((x) => `${x.node}/${x._id}`));
      const add = own(objects(j).map(finished)).filter((x) => !seen.has(`${x.node}/${x._id}`));
      rows = rows.concat(add); if (!add.length) total = rows.length;
      tbl.update(rows); drawMore();
    } catch (e) { toast(e.message, "bad"); }
  }

  /** A server without a log store: its own latest and slowest, in memory. */
  async function ring(my) {
    clear(note, h("div", { class: "banner warn", style: { marginBottom: "10px" } }, icon("warn"), h("div", { class: "grow" }, h("b", {}, "This server keeps no log store"), h("div", { class: "small" }, "These are the latest 500 queries it ran (a few seconds at a busy server) and its slowest. Start it with --log-store to keep 7 days of every slow and failed query and a sample of the rest."))));
    asSql.removeAttribute("href");
    try {
      const [a, b] = await Promise.all([api("/v1/queries/recent?limit=500"), api("/v1/queries/slow?limit=200").catch(() => ({ queries: [] }))]);
      if (my !== gen) return;
      const seen = new Set(), all = [];
      for (const x of [...a.queries, ...b.queries]) if (!seen.has(x.id)) { seen.add(x.id); all.push(x); }
      const t = span();
      rows = own(all.filter((x) => x.ts_ms >= t.from && (!t.to || x.ts_ms < t.to)
        && (f.preset !== "failed" || x.outcome !== "ok") && (f.preset !== "slow" || x.ms >= slowMs())
        && (!f.q || x.sql.toLowerCase().includes(f.q.toLowerCase())) && (!f.fingerprint || x.fingerprint === f.fingerprint)
        && (!f.key || x.key === f.key) && (!f.request || x.request_id === f.request) && (f.min === "" || x.ms >= +f.min) && (f.parts || !x.shard))).sort((p, q2) => q2.ts_ms - p.ts_ms);
      total = rows.length;
      const byMin = new Map();
      for (const x of rows) { const k = Math.floor(x.ts_ms / 60e3) + "|" + x.outcome; byMin.set(k, (byMin.get(k) || 0) + 1); }
      histogram(hist, [...byMin].map(([k, n]) => { const [m, o] = k.split("|"); return [+m, o, n]; }), { unit: 1, t });
      tbl.update(rows);
      count.textContent = `${fmt.plural(total, "query", "queries")} in memory`;
      setCount("history", "in memory");
      clear(more);
    } catch (e) { if (my === gen) { clear(note, failure(e, "The latest queries")); tbl.update([]); } }
  }

  (async () => { store = await hasLogStore(); if (store) cols = await queryColumns(); refresh(); })();
  const stops = [poll(5000, () => { if (!rows.length || (span().to == null && document.querySelector(".drawer") == null)) return refresh(); }), bus.on("range", () => refresh())];
  return () => stops.forEach((x) => x());
}

/** Every call of statements, by how it ended: {calls, server, client, deadline}. */
function servedOf(list) {
  const o = { calls: 0, server: 0, client: 0, deadline: 0 };
  for (const x of list) {
    const sp = split(x.outcomes, x.calls);
    o.server += sp.server; o.client += sp.client; o.deadline += x.outcomes?.client_deadline || 0;
    o.calls += x.calls;
  }
  return o;
}

/** Counts over time as stacked bars by outcome: rows of [bucket, outcome, n], buckets in minutes (or hours) since 1970. */
function histogram(box, rows, { unit, t }) {
  const to = t.to ?? Date.now(), spanMin = Math.max(1, (to - t.from) / 60e3);
  const binMin = Math.max(unit, Math.ceil(spanMin / 120 / unit) * unit);
  const t0 = Math.floor(t.from / 60e3 / binMin) * binMin, n = Math.max(1, Math.ceil((to / 60e3 - t0) / binMin));
  const bins = Array.from({ length: n }, () => ({}));
  const totals = {};
  for (const [b, k, c] of rows) {
    if (b == null) continue;
    const i = Math.floor((b * unit - t0) / binMin);
    if (i < 0 || i >= n) continue;
    bins[i][k] = (bins[i][k] || 0) + c; totals[k] = (totals[k] || 0) + c;
  }
  const keys = [...OUTCOME_ORDER.filter((k) => totals[k]).reverse(), ...Object.keys(totals).filter((k) => !OUTCOME_ORDER.includes(k))];
  const max = Math.max(1, ...bins.map((b) => Object.values(b).reduce((a, x) => a + x, 0)));
  const W = 1000, H = 70, bw = W / n;
  const svgEl = s("svg", { viewBox: `0 0 ${W} ${H}`, style: { width: "100%", height: H + "px", display: "block" }, preserveAspectRatio: "none", role: "img", "aria-label": "queries over time by outcome" });
  const tAt = (i) => (t0 + i * binMin) * 60e3;
  bins.forEach((b, i) => {
    let y = H;
    for (const k of keys) {
      const v = b[k] || 0; if (!v) continue;
      const hh = Math.max(1, v / max * (H - 4)); y -= hh;
      const rc = s("rect", { x: i * bw + 0.5, y, width: Math.max(1, bw - 1), height: hh, style: { fill: outcomeOf(k).color } });
      const tip = (e) => showTip(e.clientX, e.clientY, [h("div", { class: "t" }, `${fmt.datetime(tAt(i))} – ${fmt.time(tAt(i + 1), binMin < 1440)}`), ...keys.filter((x) => b[x]).reverse().map((x) => h("div", { class: "r" }, h("span", { class: "k box", style: { background: outcomeOf(x).color } }), h("b", {}, fmt.n(b[x])), h("span", { class: "n" }, outcomeOf(x).label)))]);
      rc.addEventListener("pointermove", tip);
      rc.addEventListener("click", tip); // a tap, on a phone
      rc.addEventListener("pointerleave", hideTip);
      svgEl.append(rc);
    }
  });
  const ticks = narrow() ? 3 : 5;
  const axis = h("div", { class: "row nw small muted", style: { justifyContent: "space-between", marginTop: "2px" } }, Array.from({ length: ticks }, (_, k) => { const i = Math.round((n - 1) * k / (ticks - 1)); return h("span", { class: "mono" }, binMin >= 1440 ? new Date(tAt(i)).toLocaleDateString() : when(tAt(i), { sec: false })); }));
  clear(box, rows.length ? [svgEl, axis, h("div", { class: "row small", style: { gap: "12px", marginTop: "4px" } }, keys.slice().reverse().map((k) => h("span", { class: "row nw", style: { gap: "5px" } }, h("span", { class: "dot", style: { background: outcomeOf(k).color } }), `${outcomeOf(k).label} ${fmt.n(totals[k])}`)))] : h("div", { class: "small muted" }, "No query in this range."));
}
