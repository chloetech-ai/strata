// Tables and storage: which table is growing, which shard is skewed or
// hot, which copy is stale, for every table at once (GET
// /v1/index/tables: sizes from every primary's segments, growth from the
// sizes the server keeps each hour, rates and lag). Four cards answer the
// questions; the list holds every table; the shards and copies of the
// table picked are drawn below, the problems first. One table's fields,
// bytes and reads are its drill-down, the index inspector
// (#/inspector?table=).

import { h, s, clear, fmt, api, card, empty, failure, icon, signInNeeded, store, bus, tipOn, hideTip, showTip, seg, ApiError, copyName, copyTitle, can } from "../core.js";
import { state } from "../data.js";
import { shardTrouble } from "../sqlguard.js";
import { tables as catalog, table as tableOf } from "../catalog.js";

export default { id: "tables", title: "Tables and storage", icon: "layers", group: "Diagnose", keywords: "tables storage size bytes rows growth growing skew skewed shard hot reads writes stale lag copies replicas disk inspector index", range: false, render, onParams };

let reselect = null;
function onParams(r) { reselect?.(r.params.get("table")); }

const OK_S = 1, WARN_S = 30;
/** A copy's lag as a level: ok (< 1 s), warn (< 30 s), bad (more, or no answer). */
const lagLevel = (x) => x == null ? "ok" : x.down || x.behind_s == null && x.behind_txn > 0 ? (x.down ? "bad" : "warn") : x.behind_s >= WARN_S ? "bad" : x.behind_s >= OK_S ? "warn" : "ok";
const pctText = (v, d = 1) => v == null || !Number.isFinite(v) ? "–" : (v > 0 ? "+" : v < 0 ? "−" : "") + fmt.pct(Math.abs(v), d);
/** Seconds as "4 s", "47 min", "2 h 5 min". */
const secs = (v) => v < 60 ? `${Math.round(v)} s` : v < 3600 ? `${Math.round(v / 60)} min` : `${Math.floor(v / 3600)} h${Math.round(v % 3600 / 60) ? ` ${Math.round(v % 3600 / 60)} min` : ""}`;
// Under this many rows a shard (on average), a few rows make any shard look
// skewed: such tables are not named as skewed.
const SKEW_ROWS = 100;
const material = (t) => (t.mean_docs || 0) >= SKEW_ROWS;
const times = (v) => v == null || !Number.isFinite(v) ? "–" : `${v.toFixed(v >= 10 ? 0 : 2).replace(/\.?0+$/, "")}×`;

/** A table's figures, from its row of /v1/index/tables, with what follows from them. */
export function derive(t) {
  const shards = t.shards || [];
  const mean = (k) => shards.length ? shards.reduce((a, x) => a + (x[k] || 0), 0) / shards.length : 0;
  const top = (k) => shards.reduce((b, x) => (x[k] || 0) > (b?.[k] || 0) ? x : b, null);
  const md = mean("docs"), mb = mean("bytes");
  const bigDocs = top("docs"), bigBytes = top("bytes");
  // Skew: the server's, else from the shards.
  const skewShard = t.skew?.shard ?? bigDocs?.shard;
  const skew = t.skew?.over_mean != null ? 1 + t.skew.over_mean : md ? (bigDocs.docs || 0) / md : null;
  const skewBytes = mb && skewShard != null ? (shards.find((x) => x.shard === skewShard)?.bytes || 0) / mb : null;
  const hot = shards.some((x) => x.reads_per_min != null) ? top("reads_per_min") : null;
  const mr = mean("reads_per_min");
  const d1 = t.growth?.d1_bytes, d7 = t.growth?.d7_bytes;
  const base = (d) => d != null && t.bytes != null && t.bytes - d > 0 ? d / (t.bytes - d) : null;
  const stale = [...(t.stale || [])].sort((a, b) => (b.down ? Infinity : b.behind_s ?? 0) - (a.down ? Infinity : a.behind_s ?? 0));
  // The worst lag of a copy that answers; copies that do not are counted apart (they are behind on every table).
  const answering = stale.filter((x) => !x.down);
  return { ...t, mean_docs: md, mean_bytes: mb, skew, skewShard, skewBytes, bigBytes, hot, hotTimes: hot && mr ? hot.reads_per_min / mr : null,
    d1, d7, g1: base(d1), g7: base(d7), worst: answering[0] || null, down: stale.length - answering.length, staleList: stale };
}

/** The window the rates are over, in words: "last 4 min", or null before the server measured one. */
const windowWords = (s) => s == null ? null : s < 90 ? `last ${Math.round(s)} s` : `last ${Math.round(s / 60)} min`;

function render(page, r) {
  let rows = null, err = null, took = null, at = null, legacy = false, pick = r.params.get("table") || store.get("tables.pick", null);
  // The rates' window (the longest of the tables'), and how often the server looks by itself.
  let win = null, lookEvery = 60;
  let by = store.get("tables.by", "docs");
  let growth = store.get("tables.growth", "d1");
  const stamp = h("span", { class: "small muted" });
  const again = h("button", { class: "btn sm", onclick: () => load() }, icon("refresh"), "Refresh");
  const growthSeg = seg([["d1", "24 h"], ["d7", "7 d"]], growth, (v) => { growth = v; store.set("tables.growth", v); draw(); }, { "aria-label": "Growth over", title: "Growth against the size a day or a week ago" });
  const sub = h("p", {}, "Which table is growing, which shard is skewed or hot, which copy is stale.");
  const cards = h("div", { class: "dt-answers" });
  const list = h("div");
  const shardsBox = h("div"), copiesBox = h("div");
  const shardsCard = card({ title: "Shards", sub: "largest first, so the outliers lead", body: shardsBox, cls: "dt-shards" });
  const copiesCard = card({ title: "Copies", sub: "replication lag of every copy, problems listed", body: copiesBox, cls: "dt-copies" });
  const rateChip = h("span", { class: "ix-chip est" });
  const listCard = card({ title: "Tables", sub: "largest growth first · click a row for its shards and copies, its name for fields, bytes and reads", body: list, flush: true, cls: "dt-list", tools: h("span", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "ix-chip exact", title: "Rows, bytes and segments are read from every segment of every primary" }, "sizes exact"), rateChip) });
  const rateNote = h("span");
  page.append(
    h("div", { class: "page-head" }, h("div", {}, h("h1", {}, "Tables and storage"), sub),
      h("div", { class: "tools" }, stamp, h("label", { class: "small muted" }, "growth over"), growthSeg, again)),
    cards, listCard,
    h("div", { class: "dt-pair" }, shardsCard, copiesCard),
    card({ title: "What the numbers are", body: h("p", { class: "small dim", style: { margin: 0 } },
      "Rows and bytes: exact, from every segment of each shard's primary; one copy's bytes, and all copies' as that times the copies a shard has. ",
      "Growth: against the size the server kept a day or a week ago (it keeps one a table each hour in its log store, so a server without --log-store shows none). ",
      rateNote,
      "Skew: the largest shard against the mean of all shards. ",
      "Lag: how far each copy is behind its primary, in writes and in seconds, as the copies last said. ",
      "The page asks each copy once for its sizes (at most 16 at a time; the server keeps the answer 30 s) and samples no documents: the drill-down of one table does that, when it is opened. It reads them again every 30 s.") }));

  async function load() {
    again.disabled = true; stamp.textContent = "Reading the sizes of every table…";
    const t0 = performance.now();
    try {
      // The sizes of every table are an admin's: another key lists the
      // tables it reads, each with its drill-down, and asks nothing it would be refused.
      if (!can("admin")) throw new ApiError(403, "the sizes need an admin key");
      const j = await api("/v1/index/tables");
      rows = (Array.isArray(j) ? j : j.tables || []).map(derive);
      win = Math.max(-1, ...rows.map((t) => t.rate_window_s ?? -1)); if (win < 0) win = null;
      lookEvery = j.look_every_s ?? 60;
      err = null; legacy = false; took = performance.now() - t0; at = Date.now();
    } catch (e) {
      if (e instanceof ApiError && (e.status === 404 || e.status === 405 || (e.status === 403 && !can("admin")))) {
        // An older server: no list of sizes; the catalog's tables, each with its drill-down.
        legacy = true; err = null;
        try { rows = (await catalog(true)).map((t) => ({ table: t.table, legacy: true })); } catch (e2) { err = e2; }
      } else err = e;
    }
    again.disabled = false;
    draw();
  }

  function draw() {
    hideTip();
    if (err) { clear(cards, failure(err, "The table sizes")); clear(list); listCard.hidden = true; return; }
    if (!rows) { clear(cards, empty("layers", "Reading the sizes of every table…")); listCard.hidden = true; return; }
    listCard.hidden = false;
    drawHead();
    if (legacy) {
      stamp.textContent = "";
      clear(cards, h("div", { class: "banner dt-done", style: { gridColumn: "1 / -1" } }, icon("info", "lg"), h("div", { class: "grow" }, ...(can("admin") ? [h("b", {}, "This server does not list the sizes of its tables"), h("div", { class: "small muted" }, "It answers GET /v1/index/tables from a newer version on. Open a table below for its fields, bytes, freshness and reads.")]
          : [h("b", {}, "The sizes of the tables are for an admin key"), h("div", { class: "small muted" }, "This key sees the tables below. Open one for its fields and rows.")]))));
      clear(list, h("div", { class: "dt-legacy" }, rows.map((t) => h("a", { class: "btn", href: "#/inspector?table=" + encodeURIComponent(t.table) }, icon("table"), t.table))));
      clear(shardsBox); clear(copiesBox); shardsCard.hidden = copiesCard.hidden = true;
      return;
    }
    shardsCard.hidden = copiesCard.hidden = false;
    stamp.textContent = `read in ${fmt.ms(took)} at ${fmt.time(at)} · again every 30 s`;
    const ww = windowWords(win);
    rateChip.textContent = ww ? `rates: ${ww}` : "rates: not measured yet";
    rateChip.title = ww ? `Writes and reads per minute over the ${ww}: each table's counts now against the server's look of then (it looks every ${lookEvery} s by itself, and keeps 5 minutes of looks)` : `The server measures rates against an earlier look of its own: it looks every ${lookEvery} s, so the first rates come within a minute or so of it starting`;
    clear(rateNote, `Writes and reads per minute: ${ww ? `over the ${ww}` : "not measured yet"}, each table's counts now against the server's look of then (it looks every ${lookEvery} s by itself and measures over up to 5 minutes; a server that restarted in the window gives none until its next look). Writes are each shard's primary's; reads are documents read, by every copy, added up over the shards. `);
    if (!rows.length) { clear(cards, empty("table", "No tables yet", "Create one in Schema, or load a file in Import and export.")); clear(list); return; }
    if (!pick || !rows.some((t) => t.table === pick)) pick = worstTable();
    drawCards(); drawList(); drawShards(); drawCopies();
  }

  function drawHead() {
    const r = reachOf();
    sub.textContent = `Which table is growing, which shard is skewed or hot, which copy is stale: ${fmt.plural(rows.length, "table")} on ${r.shards > 1 ? `${fmt.n(r.shards)} shards × ${r.copies} copies` : "one server"}.`;
  }
  const reachOf = () => {
    const sh = state.cluster?.shards || state.health?.shards || [];
    const copies = sh.length ? Math.max(...sh.map((x) => x.replicas?.length || 1)) : 1;
    const shards = Math.max(sh.length, ...rows.map((t) => t.shards?.length || 0), 1);
    return { shards, copies, total: sh.reduce((a, x) => a + (x.replicas?.length || 1), 0) || shards * copies };
  };
  // Copies the cluster says do not answer. /v1/index/tables lists the copies
  // that answer and are behind; one that does not answer holds every
  // table's rows of its shard, so it counts for each table.
  const downCopies = () => (state.cluster?.shards || state.health?.shards || []).flatMap((sh, i) => (sh.replicas || []).map((rep, j) => ({ rep, shard: sh.shard ?? i, j })))
    .filter(({ rep }) => rep.status && rep.status !== "ok" || rep.role === "unknown")
    .map(({ rep, shard, j }) => ({ copy: rep.short || `s${shard}-${j}`, url: rep.url, shard, down: true, why: rep.avoided_why || rep.error || "does not answer", since: rep.down_since_ms }));
  const g = (t) => growth === "d7" ? t.g7 : t.g1;
  const gb = (t) => growth === "d7" ? t.d7 : t.d1;
  const worstTable = () => [...rows].sort((a, b) => (b.staleList.length - a.staleList.length) || ((material(b) ? b.skew || 0 : 0) - (material(a) ? a.skew || 0 : 0)))[0]?.table;
  const open = (t, extra = {}) => "#/inspector?" + new URLSearchParams({ table: t, ...extra });

  // ------------------------------------------------------------- the four answers
  function drawCards() {
    const ans = (q, title, body, foot, tone = "") => h("section", { class: "card dt-ans " + tone }, h("div", { class: "small muted dt-q" }, q), h("div", { class: "dt-a" }, title), h("div", { class: "small dim dt-why" }, body), foot ? h("div", { class: "dt-foot" }, foot) : null);
    const link = (href, text) => h("a", { class: "btn sm", href }, text);
    // Growing.
    const withG = rows.filter((t) => g(t) != null);
    const grow = [...withG].sort((a, b) => g(b) - g(a))[0];
    const per = growth === "d7" ? "a week" : "a day";
    const c1 = !withG.length
      ? ans("Is a table growing?", "No size history yet", "The server keeps each table's size every hour in its log store; growth shows once a day (or week) of it is there. A server without --log-store keeps none.")
      : g(grow) <= 0.001 ? ans("Is a table growing?", "No: every table is flat", `None grew more than 0.1% ${per}.`)
        : ans("Is a table growing?", `${grow.table} ${pctText(g(grow))} ${per}`,
          `${gb(grow) >= 0 ? "+" : "−"}${fmt.bytes(Math.abs(gb(grow)))} in ${growth === "d7" ? "7 days" : "24 h"} on one copy (${fmt.bytes(grow.bytes)} now)${grow.writes_per_min != null ? `, ${fmt.si(grow.writes_per_min)} writes/min` : ""}.${tableOf(grow.table)?.short?.retain ? ` It keeps rows ${tableOf(grow.table).short.retain}.` : ""}`,
          link(open(grow.table), `Open ${grow.table}`), g(grow) > 0.05 ? "warn" : "");
    // Skewed.
    const cands = rows.filter((t) => t.skew != null && (t.shards?.length || 0) > 1);
    const sk = cands.filter(material).sort((a, b) => b.skew - a.skew)[0];
    const small = cands.filter((t) => !material(t)).length;
    const smallNote = small ? ` ${fmt.plural(small, "table")} under ${SKEW_ROWS} rows a shard ${small === 1 ? "is" : "are"} left out: a few rows make any shard look skewed.` : "";
    const c2 = !cands.length ? ans("Is a shard skewed?", "One shard: nothing to compare", "Skew compares the shards of a cluster.")
      : !sk ? ans("Is a shard skewed?", "No table is large enough to tell", smallNote.trim())
      : sk.skew < 1.25 ? ans("Is a shard skewed?", "No: every table is even", `The largest shard of any table is ${times(sk.skew)} the mean (${sk.table}, shard ${sk.skewShard}).${smallNote}`)
        : ans("Is a shard skewed?", `${sk.table}: shard ${sk.skewShard} holds ${times(sk.skew)} the mean`,
          `${fmt.si(sk.shards.find((x) => x.shard === sk.skewShard)?.docs)} rows against a mean of ${fmt.si(sk.mean_docs)}${sk.skewBytes ? `; its bytes are ${times(sk.skewBytes)} the mean` : ""}.${tableOf(sk.table)?.short?.shard_by ? ` Rows are placed by ${tableOf(sk.table).short.shard_by}: one value may hold many rows.` : ""}`,
          link(open(sk.table, { shard: sk.skewShard }), `See shard ${sk.skewShard}`), "warn");
    // Hot.
    const totalReads = rows.reduce((a, t) => a + (t.reads_per_min || 0), 0);
    const hotT = [...rows].filter((t) => t.reads_per_min != null).sort((a, b) => b.reads_per_min - a.reads_per_min)[0];
    const ww = windowWords(win);
    const c3 = !hotT ? ans("Is a shard hot?", "Not measured yet", `The server measures reads a minute against its look of a minute or more ago; it looks every ${lookEvery} s by itself. This page reads again every 30 s.`)
      : !totalReads ? ans("Is a shard hot?", "No: nothing was read", `No table's documents were read in the ${ww || "last minutes"}.`)
      : hotT.hot && hotT.hotTimes > 1.5 ? ans("Is a shard hot?", `${hotT.table}: shard ${hotT.hot.shard} serves ${times(hotT.hotTimes)} the mean reads`, `${fmt.si(hotT.hot.reads_per_min)} reads/min against a mean of ${fmt.si(hotT.reads_per_min / (hotT.shards.length || 1))}, over the ${ww || "last minutes"}. ${fmt.pct(hotT.reads_per_min / totalReads, 0)} of all reads go to ${hotT.table}.`, link(open(hotT.table, { shard: hotT.hot.shard }), `See shard ${hotT.hot.shard}`), "warn")
        : ans("Is a shard hot?", hotT.hot ? `No: shard ${hotT.hot.shard} is the busiest, at ${times(hotT.hotTimes)} the mean` : `${hotT.table} takes ${fmt.pct(hotT.reads_per_min / totalReads, 0)} of reads`, `${hotT.hot ? `Of ${hotT.table}, the most read table (${fmt.pct(hotT.reads_per_min / totalReads, 0)} of reads): ${fmt.si(hotT.hot.reads_per_min)} reads/min on shard ${hotT.hot.shard} against a mean of ${fmt.si(hotT.reads_per_min / (hotT.shards.length || 1))}` : `${fmt.si(hotT.reads_per_min)} reads/min of ${fmt.si(totalReads)} over all tables. The drill-down shows its reads by shard`}, over the ${ww || "last minutes"}.`, link(open(hotT.table), `Open ${hotT.table}`));
    // Stale.
    const copies = new Map();
    for (const t of rows) for (const x of t.staleList) { const k = name(x.copy); const old = copies.get(k); if (!old || (x.behind_s ?? Infinity) > (old.behind_s ?? Infinity)) copies.set(k, { ...x, table: t.table }); }
    for (const d of downCopies()) copies.set(name(d.copy), d);
    const level = (x) => x.down ? 3 : lagLevel(x) === "bad" ? 2 : lagLevel(x) === "warn" ? 1 : 0;
    const st = [...copies.values()].sort((a, b) => level(b) - level(a) || (b.behind_s ?? b.behind_txn ?? 0) - (a.behind_s ?? a.behind_txn ?? 0));
    const total = reachOf().total;
    const c4 = !st.length ? ans("Is a copy stale?", `No: all ${fmt.n(total)} copies are current`, "Every copy is under 1 s behind its primary.")
      : ans("Is a copy stale?", `${fmt.n(st.length)} of ${fmt.n(total)} copies`, [`Every other copy is under ${OK_S} s behind its primary.`,
        h("div", { class: "dt-stale" }, st.slice(0, 3).map((x) => h("span", { class: "dt-sc " + lagLevel(x), title: copyTitle(x.url || x.copy) }, `${name(x.copy)} · ${lagText(x)}`)), st.length > 3 ? h("span", { class: "small muted" }, `and ${st.length - 3} more`) : null)],
        h("a", { class: "btn sm", href: `#/cluster?copy=${encodeURIComponent(st[0].url || st[0].copy)}` }, "Open in Cluster"), st.some((x) => lagLevel(x) === "bad") ? "bad" : "warn");
    clear(cards, c1, c2, c3, c4);
  }
  const name = (c) => /^s\d+-\d+$/.test(c) ? c : copyName(c);
  const lagText = (x) => x.down ? "no answer" : x.behind_s != null ? `${secs(x.behind_s)} behind` : `${fmt.si(x.behind_txn)} writes behind`;

  // ------------------------------------------------------------- the list
  function drawList() {
    const max = Math.max(...rows.map((t) => Math.abs(g(t) ?? 0)));
    const copies = reachOf().copies;
    // A copy that does not answer is a copy of every table on its shard:
    // said once above the rows, not on each.
    const downAll = downCopies();
    const saidAbove = (x) => downAll.some((d) => d.copy === x.copy || d.url === x.copy || d.copy === name(x.copy));
    const note = (t) => { const sh = tableOf(t.table)?.short; return [sh?.retain && `keeps ${sh.retain}`, sh?.shard_by && `shard by ${sh.shard_by}`, sh?.expire_by && `expires by ${sh.expire_by}`].filter(Boolean).join(" · "); };
    const cols = [
      { key: "table", label: "Table", render: (t) => h("div", {}, h("a", { class: "wl-link", href: open(t.table), title: `Fields, bytes, freshness and reads of ${t.table}`, onclick: (e) => e.stopPropagation() }, h("b", {}, t.table)), note(t) ? h("div", { class: "tiny muted" }, note(t)) : null) },
      { key: "docs", label: "Rows", num: true, render: (t) => fmt.si(t.docs) },
      { key: "bytes", label: "On disk, one copy", short: "On disk", num: true, render: (t) => h("div", {}, fmt.bytes(t.bytes), copies > 1 ? h("div", { class: "tiny muted" }, `${fmt.bytes(t.bytes * copies)} all copies`) : null) },
      { key: "g", label: growth === "d7" ? "Δ 7 d" : "Δ 24 h", num: true, sort: (t) => g(t) ?? -Infinity, render: (t) => g(t) == null ? h("span", { class: "muted", title: "No size kept that long ago" }, "–") : h("span", { class: "dt-delta" + (g(t) === max && max > 0.01 ? " top" : ""), title: `${gb(t) >= 0 ? "+" : "−"}${fmt.bytes(Math.abs(gb(t)))} on one copy` }, pctText(g(t))) },
      { key: "g7", label: growth === "d7" ? "Δ 24 h" : "Δ 7 d", num: true, sort: (t) => (growth === "d7" ? t.g1 : t.g7) ?? -Infinity, render: (t) => h("span", { class: "dim" }, pctText(growth === "d7" ? t.g1 : t.g7)) },
      { key: "writes_per_min", label: "Writes/min", title: `Documents written per minute over the ${windowWords(win) || "last minutes (not measured yet)"}`, num: true, render: (t) => t.writes_per_min == null ? h("span", { class: "muted", title: "Not measured yet: the server needs a look of a minute ago" }, "–") : fmt.si(t.writes_per_min) },
      { key: "reads_per_min", label: "Reads/min", title: `Documents read per minute over the ${windowWords(win) || "last minutes (not measured yet)"}`, num: true, render: (t) => t.reads_per_min == null ? h("span", { class: "muted", title: "Not measured yet: the server needs a look of a minute ago" }, "–") : fmt.si(t.reads_per_min) },
      { key: "segments", label: "Segments", num: true, render: (t) => fmt.n(t.segments) },
      { key: "skew", label: "Largest shard", num: true, title: "The largest shard's rows against the mean of the shards", render: (t) => t.skew == null || (t.shards?.length || 0) < 2 ? h("span", { class: "muted" }, "–") : h("span", { class: t.skew >= 1.25 && material(t) ? "dt-flag" : "", title: `shard ${t.skewShard}${material(t) ? "" : ` · under ${SKEW_ROWS} rows a shard: not counted as skewed`}` }, times(t.skew), h("span", { class: "tiny muted" }, ` s${t.skewShard}`)) },
      { key: "lag", label: "Worst copy lag", num: true, title: "The copy furthest behind its primary, of those that answer; copies that do not answer are counted apart", sort: (t) => t.worst ? t.worst.behind_s ?? 0 : -1, render: (t) => h("span", { class: "row nw", style: { gap: "6px", justifyContent: "flex-end" } },
        t.worst ? h("span", { class: "dt-sc " + lagLevel(t.worst), title: copyTitle(t.worst.copy) }, `${name(t.worst.copy)} · ${lagText(t.worst)}`) : h("span", { class: "muted" }, "under 1 s"),
        (() => { const down = t.staleList.filter((x) => x.down && !saidAbove(x)); return down.length ? h("span", { class: "dt-sc bad", title: down.map((x) => name(x.copy)).join(", ") + (down.length === 1 ? " does" : " do") + " not answer" }, `+${down.length} down`) : null; })()) },
    ];
    const down = downAll;
    const trouble = shardTrouble();
    clear(list, down.length || trouble.length ? h("div", { class: "banner bad dt-banner dt-downall" }, icon("bad"), h("div", { class: "grow small" },
      down.length ? [h("b", {}, `${down.map((d) => d.copy).join(", ")} ${down.length === 1 ? "does" : "do"} not answer`),
        ` · every table's copy on ${down.length === 1 ? "it" : "them"} is out of date until ${down.length === 1 ? "it is" : "they are"} back; the lag below is of the copies that answer.`] : null,
      trouble.map((x) => h("div", {}, h("b", {}, `Shard ${x.shard} ${x.words}`), x.why ? ` · ${x.why}.` : "", ` ${!x.reads && !x.writes ? "Queries and writes" : !x.reads ? "Queries" : "Writes"} of every table that reach it fail until it is back.`)))) : null,
      tableEl(cols));
  }
  function tableEl(cols) {
    let sortKey = store.get("tables.sort", "g"), desc = true;
    const thead = h("thead"), tbody = h("tbody");
    const val = (c, t) => c.sort ? c.sort(t) : t[c.key];
    function head() { clear(thead, h("tr", {}, cols.map((c) => { const th = h("th", { class: (c.num ? "num " : "") + "sort", title: c.title }, c.label, sortKey === c.key ? h("span", { class: "arrow" }, desc ? "▼" : "▲") : null); th.onclick = () => { if (sortKey === c.key) desc = !desc; else { sortKey = c.key; desc = !!c.num; } store.set("tables.sort", sortKey); head(); body(); }; return th; }))); }
    function body() {
      const c = cols.find((x) => x.key === sortKey) || cols[3];
      const rs = [...rows].sort((a, b) => { const x = val(c, a), y = val(c, b); const r = x == null ? 1 : y == null ? -1 : typeof x === "number" ? x - y : String(x).localeCompare(String(y)); return desc ? -r : r; });
      clear(tbody, rs.map((t) => {
        const tr = h("tr", { class: "click" + (t.table === pick ? " dt-sel" : ""), tabindex: 0, "aria-selected": String(t.table === pick) }, cols.map((c) => h("td", { class: (c.num ? "num " : "") + "dt-c-" + c.key, "data-label": c.short || c.label }, c.render(t))));
        const sel = () => { pick = t.table; store.set("tables.pick", pick); body(); drawShards(); drawCopies(); };
        tr.onclick = (e) => { if (!e.target.closest("a,button")) sel(); };
        tr.onkeydown = (e) => { if (e.key === "Enter") sel(); };
        return tr;
      }));
    }
    head(); body();
    // On a phone the rows stack and the header goes: sort from a list instead.
    const sortSel = h("select", { class: "select sm", "aria-label": "Sort the tables by", onchange: (e) => { sortKey = e.target.value; desc = !!cols.find((c) => c.key === sortKey)?.num; store.set("tables.sort", sortKey); head(); body(); } },
      cols.map((c) => h("option", { value: c.key, selected: c.key === sortKey }, c.num ? `${c.label}, largest first` : c.label)));
    return h("div", {}, h("label", { class: "dt-sortsel small muted" }, "sort by", sortSel), h("div", { class: "tbl-wrap" }, h("table", { class: "t dt-tbl" }, thead, tbody)));
  }
  reselect = (t) => { if (t && rows?.some((x) => x.table === t)) { pick = t; draw(); } };

  // ------------------------------------------------------------- shards of the table picked
  function drawShards() {
    const t = rows.find((x) => x.table === pick);
    shardsCard.querySelector(".hd h3").textContent = `Shards of ${t.table}`;
    const shards = t.shards || [];
    if (shards.length < 2) { clear(shardsBox, empty("topology", "One shard", "Skew compares the shards of a cluster.")); return; }
    const hasReads = shards.some((x) => x.reads_per_min != null);
    if (by === "reads" && !hasReads) by = "docs";
    const unit = by === "bytes" ? fmt.bytes : fmt.si;
    const vals = shards.map((x) => ({ shard: x.shard, v: x[by === "reads" ? "reads_per_min" : by] || 0 })).sort((a, b) => b.v - a.v);
    const mean = vals.reduce((a, x) => a + x.v, 0) / vals.length || 1;
    const W = 600, H = 190, pad = { l: 34, r: 8, t: 22, b: 30 };
    const maxR = Math.max(2, ...vals.map((x) => x.v / mean)) * 1.08;
    const bw = (W - pad.l - pad.r) / vals.length;
    const Y = (ratio) => H - pad.b - ratio / maxR * (H - pad.t - pad.b);
    const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, width: "100%", class: "ix-svg dt-bars", role: "img", "aria-label": `Shards of ${t.table} by ${by}, largest first` });
    // The band within 25% of the mean, the mean, and the axis.
    svg.append(s("rect", { x: pad.l, y: Y(1.25), width: W - pad.l - pad.r, height: Y(0.75) - Y(1.25), class: "dt-band-even" }));
    for (const v of [0, 1, 2, 4, 6, 8].filter((x) => x <= maxR)) svg.append(s("text", { x: pad.l - 6, y: Y(v) + 4, "text-anchor": "end" }, `${v}×`));
    svg.append(s("line", { x1: pad.l, x2: W - pad.r, y1: Y(1), y2: Y(1), class: "dt-mean" }), s("text", { x: W - pad.r, y: Y(1) - 4, "text-anchor": "end", class: "dt-mean-lbl" }, `mean ${unit(mean)} a shard`));
    const within = vals.filter((x) => Math.abs(x.v / mean - 1) <= 0.25).length;
    vals.forEach((x, i) => {
      const ratio = x.v / mean, out = Math.abs(ratio - 1) > 0.25;
      const bar = s("rect", { x: pad.l + i * bw + (bw > 3 ? 0.5 : 0), y: Y(ratio), width: Math.max(0.8, bw - (bw > 3 ? 1 : 0)), height: Math.max(0.5, H - pad.b - Y(ratio)), class: "dt-bar" + (ratio > 1.25 ? " hi" : ratio < 0.75 ? " lo" : ""), tabindex: out ? 0 : -1, style: { cursor: "pointer" } });
      bar.onclick = () => { location.hash = open(t.table, { shard: x.shard }); };
      tipOn(bar, () => [h("div", { class: "t" }, `shard ${x.shard}`), h("div", { class: "r" }, h("span", {}, by === "reads" ? "reads/min" : by), h("span", { class: "n" }, unit(x.v))), h("div", { class: "r" }, h("span", {}, "against the mean"), h("span", { class: "n" }, times(ratio))), h("div", { class: "r" }, h("span", { class: "muted" }, "click for its drill-down"))]);
      svg.append(bar);
    });
    // The outliers, named.
    const outs = vals.map((x, i) => ({ ...x, i, ratio: x.v / mean })).filter((x) => x.ratio > 1.25).slice(0, 3);
    // Stacked at the top, right of their bars, above the shards near the mean.
    outs.forEach((x, k) => svg.append(s("text", { x: Math.min(W - 160, pad.l + (x.i + 1) * bw + 6), y: pad.t - 8 + k * 13, class: "dt-out" }, `s${x.shard} · ${times(x.ratio)} · ${unit(x.v)}`)));
    svg.append(s("text", { x: pad.l, y: H - 10 }, `${fmt.n(vals.length)} shards, largest first →`), s("text", { x: W - pad.r, y: H - 10, "text-anchor": "end" }, `within ±25% of the mean: ${fmt.n(within)} of ${fmt.n(vals.length)}`));
    const opts = [["docs", "Rows"], ["bytes", "Bytes"]].concat(hasReads ? [["reads", "Reads/min"]] : []);
    clear(shardsBox, h("div", { class: "row", style: { marginBottom: "6px" } }, seg(opts, by, (v) => { by = v; store.set("tables.by", v); drawShards(); }, { "aria-label": "Compare the shards by" }), h("span", { class: "sep" }),
      outs.length ? h("span", { class: "small" }, "outliers: ", outs.map((x, k) => [k ? ", " : "", h("a", { href: open(t.table, { shard: x.shard }) }, `shard ${x.shard}`)])) : h("span", { class: "small muted" }, "no shard over 1.25× the mean")),
      svg, h("div", { class: "tiny muted" }, `Each bar is a shard's ${by === "bytes" ? "bytes" : by === "reads" ? "reads per minute" : "rows"} against the mean (1×); the band is within 25% of it. Click a bar for that shard's copies, fields and reads.`));
  }

  // ------------------------------------------------------------- copies of the table picked
  function drawCopies() {
    const t = rows.find((x) => x.table === pick);
    copiesCard.querySelector(".hd h3").textContent = `Copies of ${t.table}`;
    const shards = state.cluster?.shards || state.health?.shards || [];
    const staleBy = new Map((t.staleList || []).map((x) => [x.copy, x]));
    const cells = [];
    shards.forEach((sh, i) => (sh.replicas || []).forEach((rep, j) => {
      const short = rep.short || `s${sh.shard ?? i}-${j}`;
      const st = staleBy.get(short) || staleBy.get(rep.url) || (rep.status && rep.status !== "ok" || rep.role === "unknown" ? { copy: short, down: true } : null);
      cells.push({ i, j, shard: sh.shard ?? i, short, url: rep.url, primary: rep.takes_writes || rep.role === "primary", st, level: lagLevel(st), why: rep.avoided_why || rep.error || (rep.role === "unknown" ? `does not answer${rep.down_since_ms ? " since " + fmt.time(rep.down_since_ms) : ""}` : null) });
    }));
    // Stale copies the cluster state does not list (a fake or an older state).
    for (const x of t.staleList || []) if (!cells.some((c) => c.short === x.copy || c.url === x.copy)) cells.push({ i: -1, j: 0, shard: null, short: name(x.copy), url: x.copy, st: x, level: lagLevel(x) });
    if (!cells.length) { clear(copiesBox, empty("topology", "One server", "Its copies are listed on a coordinator.")); return; }
    const n = shards.length || 1, maxC = Math.max(1, ...shards.map((x) => x.replicas?.length || 1));
    const W = 600, top = 4, lab = 18, rowH = 14, H = top + maxC * (rowH + 3) + 20;
    const cw = (W - lab) / n;
    const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, width: "100%", class: "ix-svg dt-strip", role: "img", "aria-label": `Lag of every copy of ${t.table}` });
    for (let j = 0; j < maxC; j++) svg.append(s("text", { x: 0, y: top + j * (rowH + 3) + rowH - 3 }, String.fromCharCode(97 + j)));
    for (const c of cells.filter((x) => x.i >= 0)) {
      const r = s("rect", { x: lab + c.i * cw + (cw > 4 ? 0.5 : 0), y: top + c.j * (rowH + 3), width: Math.max(0.8, cw - (cw > 4 ? 1 : 0.2)), height: rowH, rx: cw > 6 ? 2 : 0, class: "dt-cell " + c.level, tabindex: c.level === "ok" ? -1 : 0 });
      tipOn(r, () => [h("div", { class: "t" }, `${c.short}${c.primary ? " · primary" : ""}`), h("div", { class: "r" }, h("span", {}, c.st ? lagText(c.st) : "under 1 s behind")), c.why ? h("div", { class: "r" }, h("span", { class: "muted" }, c.why)) : null, h("div", { class: "r" }, h("span", { class: "muted mono" }, c.url || ""))]);
      svg.append(r);
    }
    const ticks = n > 1 ? [0, Math.floor(n / 2), n - 1] : [0];
    const bad = cells.filter((c) => c.level !== "ok");
    for (const i of new Set([...ticks, ...bad.filter((c) => c.i >= 0).slice(0, 3).map((c) => c.i)])) svg.append(s("text", { x: lab + i * cw + cw / 2, y: H - 4, "text-anchor": "middle", class: bad.some((c) => c.i === i) ? "dt-out" : "" }, `s${shards[i]?.shard ?? i}`));
    const legend = h("div", { class: "row small muted", style: { gap: "12px" } }, h("span", { class: "dt-key ok" }, "under 1 s"), h("span", { class: "dt-key warn" }, "1–30 s"), h("span", { class: "dt-key bad" }, "over 30 s or no answer"));
    const problems = bad.sort((a, b) => (a.level === "bad" ? 0 : 1) - (b.level === "bad" ? 0 : 1) || (b.st?.behind_s ?? Infinity) - (a.st?.behind_s ?? Infinity));
    const tbl = problems.length ? h("table", { class: "t compact dt-ptbl" }, h("thead", {}, h("tr", {}, h("th", {}, "Copy"), h("th", {}, "Behind"), h("th", {}, "Why, as far as the server knows"), h("th", {}, ""))),
      h("tbody", {}, problems.slice(0, 12).map((c) => h("tr", {},
        h("td", { class: "mono dt-nw", title: c.url || "" }, c.short),
        h("td", {}, h("span", { class: "dt-sc " + c.level }, c.st ? lagText(c.st) : "–")),
        h("td", { class: "small dim" }, c.why || (c.st?.behind_txn != null ? `${fmt.n(c.st.behind_txn)} writes behind its primary` : "")),
        h("td", { class: "num dt-nw" }, h("a", { class: "btn sm ghost", href: `#/cluster?copy=${encodeURIComponent(c.short)}` }, "Cluster"), h("a", { class: "btn sm ghost", href: `#/logs?q=${encodeURIComponent(c.short)}` }, "Logs"))))))
      : h("div", { class: "small muted", style: { marginTop: "8px" } }, `All ${fmt.n(cells.length)} copies are under 1 s behind their primaries.`);
    clear(copiesBox, legend, svg, problems.length > 12 ? h("div", { class: "small muted" }, `${problems.length - 12} more copies behind; the worst are listed.`) : null, tbl);
  }

  // The cluster's shape (shards, copies, which answer) comes every few seconds.
  let shape = "";
  let downSeen = "";
  const offs = [bus.on("cluster", () => {
    if (!rows || legacy || err || !rows.length) return;
    const r = reachOf(), now = `${r.shards}x${r.copies}x${r.total}`;
    // Redraw what the copies' state changes, only when it changed.
    const downNow = downCopies().map((d) => d.copy).join(",") + "|" + shardTrouble().map((x) => x.shard + x.words).join(",");
    if (now !== shape) { shape = now; downSeen = downNow; draw(); } else if (downNow !== downSeen) { downSeen = downNow; drawCards(); drawList(); drawCopies(); } else { drawCards(); drawCopies(); }
  })];
  draw();
  load();
  // Again every 30 s (the server keeps an answer 30 s), unless paused or hidden.
  const again30 = setInterval(() => { if (!state.paused && !signInNeeded() && !document.hidden && page.isConnected && !again.disabled) load(); }, 30e3);
  return () => { clearInterval(again30); hideTip(); reselect = null; offs.forEach((f) => f()); };
}
