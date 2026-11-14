// One table in depth, the drill-down of Tables and storage
// (#/inspector?table=products&shard=42): what it holds, how big each part
// of it is, how fresh it is, and how often its documents and keys are
// read, for the whole cluster, one shard or one copy (GET /v1/index). A
// shard's data is its primary's; its reads are its copies' added up. Sizes
// and fill come from a sample of documents; documents, bytes and terms
// from every segment; reads are approximate counts kept in memory since
// each server started. At scale (192 shards x 3) the copies with a problem
// come first and the rest fold into a grid of shards.

import { h, s, clear, fmt, card, api, empty, failure, store, showTip, hideTip, tipOn, icon, table, confirm, toast, seg, copyName, copyTitle } from "../core.js";

export default { id: "inspector", title: "Table details", icon: "layers", group: "Diagnose", parent: "tables", nav: false, range: false, keywords: "index inspector table details statistics fields documents size bytes distribution histogram freshness last write segments retrieval reads keys coverage hot shards replicas skew", render, onParams };

let reopen = null;
function onParams(r) { reopen?.(r.params); }
/** More shards than this and the map folds into a grid of shards, problems first. */
const MANY = 8;

// A field's color by what its index is; the store's and the rest's.
const KIND = { text: "var(--s1)", sparse: "var(--s1)", keyword: "var(--s2)", int: "var(--s3)", float: "var(--s3)", timestamp: "var(--s3)", bool: "var(--s3)", vector: "var(--s7)", json: "var(--s4)", edges: "var(--s8)" };
const GROUP = { text: "text", sparse: "text", keyword: "keyword", int: "column", float: "column", timestamp: "column", bool: "column", vector: "vector", json: "JSON", edges: "edges" };
const LEGEND = [["text index", "var(--s1)"], ["keyword index", "var(--s2)"], ["columns", "var(--s3)"], ["vectors", "var(--s7)"], ["JSON", "var(--s4)"], ["edges", "var(--s8)"], ["doc store (compressed share)", "var(--s5)"], ["primary key and the rest", "var(--s6)"], ["segment overhead", "var(--axis)"]];
const STORE = "var(--s5)", OTHER = "var(--s6)", OVER = "var(--axis)";
// Fills light enough to want dark text on them.
const LIGHT = new Set(["var(--s4)", "var(--s5)", "var(--axis)"]);

const chip = (cls, text, title) => h("span", { class: "ix-chip " + cls, title }, text);
const exact = () => chip("exact", "exact", "Read from every segment");
const sampled = (n) => chip("est", "sampled", n ? `From ${fmt.n(n)} documents drawn at random` : "From documents drawn at random");
const approx = () => chip("est", "approximate", "Counted in memory with bounded sketches since each server started");
const tip = (title, rows) => [h("div", { class: "t" }, title), ...rows.map(([k, v, c]) => h("div", { class: "r" }, c ? h("span", { class: "k box", style: { background: c } }) : null, h("span", {}, k), h("span", { class: "n" }, v)))];
const svgBox = (w, ht, label) => s("svg", { viewBox: `0 0 ${w} ${ht}`, width: "100%", role: "img", "aria-label": label, class: "ix-svg" });

function render(page, r) {
  let data = null, tables = [], listed = false, loading = false, err = null, lag = null;
  let scope = store.get("inspector.scope", { kind: "cluster" });
  // A link names the table, and maybe a shard (by its number) or a copy.
  let want = { table: r?.params?.get("table") || null, shard: r?.params?.get("shard"), copy: r?.params?.get("copy") };
  if (want.table) store.set("inspector.table", want.table);
  let what = store.get("inspector.what", "docs");
  let sample = store.get("inspector.sample", 2000);
  const pick = h("select", { class: "select sm", "aria-label": "Table", onchange: () => { store.set("inspector.table", pick.value); scope = { kind: "cluster" }; load(); } });
  const sizes = h("select", { class: "select sm", "aria-label": "Documents sampled per server", onchange: () => { sample = +sizes.value; store.set("inspector.sample", sample); load(); } },
    [2000, 10000, 50000].map((n) => h("option", { value: n, selected: n === sample }, `sample ${fmt.si(n)} per server`)));
  const again = h("button", { class: "btn sm", onclick: () => load() }, icon("refresh"), "Refresh");
  const reset = h("button", { class: "btn sm", onclick: resetReads, title: "Start the read counts again" }, icon("reset"), "Reset reads");
  const stamp = h("span", { class: "small muted" });
  const map = h("div", { class: "ix-scope" }), tiles = h("div", { class: "ix-tiles" });
  const bytes = h("div"), docs = h("div"), fields = h("div"), fresh = h("div"), skew = h("div"), cover = h("div"), curve = h("div"), freq = h("div"), topDocs = h("div"), topKeys = h("div");
  // R2-data-tools-1: whose reads these are, drawn once the answer is in.
  const readsTail = h("span"), partial = h("div", { class: "ix-c12", hidden: true });
  const whatSeg = h("span");
  const body = h("div", { class: "ix-grid" },
    card({ title: "Where the bytes go", sub: "each box is a field's index or its share of the doc store, on disk", tools: exact(), body: bytes, cls: "ix-c7" }),
    card({ title: "Document size", sub: "stored bytes per document, before compression", tools: sampled(), body: docs, cls: "ix-c5" }),
    card({ title: "Fields", sub: "how many documents have each, its value sizes, its index on disk", body: fields, cls: "ix-c12" }),
    card({ title: "Freshness", sub: "the last write each copy took", body: fresh, cls: "ix-c6" }),
    card({ title: "Shards compared", sub: "how documents, bytes and reads spread over the shards", body: skew, cls: "ix-c6" }),
    partial,
    card({ title: "Read at least once", sub: "since each server started", tools: whatSeg, body: cover, cls: "ix-c4" }),
    card({ title: "How concentrated reads are", body: curve, cls: "ix-c4" }),
    card({ title: "Times each was read", body: freq, cls: "ix-c4" }),
    card({ title: "Most retrieved documents", sub: "rows returned in query answers, since each server started", tools: approx(), body: topDocs, cls: "ix-c6" }),
    card({ title: "Most requested keys", sub: "reads by key (GET /v1/kv, MGET, Get), since each server started", tools: approx(), body: topKeys, cls: "ix-c6" }));
  const content = h("div", {}, card({ title: "Where to look", sub: "the cluster, a shard or a copy: every number below follows", body: map, attrs: { style: { marginBottom: "14px" } } }), tiles, body,
    h("p", { class: "small muted ix-foot" }, "Documents, bytes, segments, index sizes and distinct terms are read from every segment (terms of large fields are estimated with a HyperLogLog sketch, which also merges across shards). Value sizes, fill and document sizes come from documents drawn at random from each server, weighted to its whole table; tokens per document come from every document's length norm, exact under 24 tokens and rounded down by at most 12.5% above. Reads are counted in memory since each server started or was reset: the top lists keep each table's 256 most read keys (Space-Saving, each count at most its error too high), coverage and the frequency chart come from a hash sample of keys that halves itself to stay under 4,096. A coordinator counts what each copy served through it, and each server what it answered its own clients, so nothing is counted twice.", readsTail));
  const title = h("h1", {}, "Table details");
  // Inside a table's home (its Storage tab) the page is a tab's body: no
  // breadcrumb, heading or table picker of its own (CCB-21).
  if (r?.embedded) page.append(h("div", { class: "row", style: { gap: "8px", flexWrap: "wrap", marginBottom: "12px" } },
    h("span", { class: "small muted", style: { flex: "1 1 360px" } }, "How big each part of the table is, how fresh, and how often its documents and keys are read (since each server started). ",
      h("a", { href: `#/inspector?table=${encodeURIComponent(want.table || "")}` }, "Open in Tables and storage")),
    h("span", { class: "row", style: { gap: "8px", flexWrap: "wrap", justifyContent: "flex-end" } }, stamp, sizes, reset, again)), content);
  else page.append(h("div", { class: "page-head" }, h("div", {}, h("div", { class: "small" }, h("a", { class: "wl-link", href: "#/tables" }, "Tables and storage"), h("span", { class: "muted" }, " › one table")), title, h("p", {}, "What the table holds, how big each part of it is, how fresh it is, and how often its documents and keys are read. Reads are counted since each server started (Reset reads starts them again).")),
    h("div", { class: "tools" }, stamp, h("label", { class: "small muted" }, "Table"), pick, sizes, reset, again)), content);

  async function loadTables() {
    try {
      tables = ((await api("/v1/tables")).tables || []).map((t) => t.table);
      listed = true;
      const name = store.get("inspector.table", null);
      clear(pick, tables.map((t) => h("option", { value: t, selected: t === name }, t)));
      if (!tables.includes(name) && tables.length) pick.value = tables[0];
    } catch { /* the index call says why */ }
  }

  async function load() {
    if (loading) return;
    loading = true; again.disabled = true; stamp.textContent = "Reading every segment…";
    try {
      const q = new URLSearchParams({ sample: String(sample), top: "25" });
      if (pick.value) q.set("table", pick.value);
      // The copies' lag in time, where the server lists table sizes (cached there 30 s).
      const [d, all] = await Promise.all([api("/v1/index?" + q), api("/v1/index/tables").catch(() => null)]);
      data = d;
      const row = (Array.isArray(all) ? all : all?.tables || []).find((x) => x.table === data.table);
      lag = row ? new Map((row.stale || []).map((x) => [x.copy, x])) : null;
      err = null;
      title.textContent = data.table;
      stamp.textContent = `computed in ${fmt.ms(data.took_ms)} at ${fmt.time(data.at_ms)}`;
      // A link's shard or copy, once the shards are known.
      if (want.shard != null || want.copy) {
        const si = data.shards?.findIndex((sh) => String(sh.shard) === String(want.shard));
        if (want.copy) {
          for (const [i, sh] of (data.shards || []).entries()) { const ri = sh.replicas.findIndex((rep) => rep.url === want.copy || copyName(rep.url) === want.copy); if (ri >= 0) scope = { kind: "replica", s: i, r: ri }; }
        } else if (si >= 0) scope = { kind: "shard", s: si };
        want = {};
      }
    } catch (e) { err = e; stamp.textContent = ""; }
    loading = false; again.disabled = false;
    draw();
  }

  /** INS-3: whose reads these are, from `reads_from` (every coordinator that answered). */
  function readsFrom() {
    const f = Array.isArray(data?.reads_from) ? data.reads_from : [];
    if (f.length < 2) return " Reset starts the counts again.";
    const ok = f.filter((x) => x.ok).length;
    return ` Reads are counted by every coordinator (${ok} of ${f.length} answered${ok < f.length ? ": " + f.filter((x) => !x.ok).map((x) => x.name || x.url).join(", ") + " did not" : ""}), and Reset starts every coordinator's counts again.`;
  }

  async function resetReads() {
    const coord = data?.role === "coordinator";
    if (!await confirm({ title: "Reset the read counts?", body: coord ? "Every coordinator's counts of what each copy served through it start again from zero. Each server keeps what it answered its own clients until it is reset there." : "This server's counts of documents retrieved and keys asked for start again from zero.", ok: "Reset", danger: true })) return;
    try { await api("/v1/index/reads/reset", { body: {} }); toast("Read counts reset"); load(); } catch (e) { toast(e.message, "bad"); }
  }

  // ------------------------------------------------------------- scopes
  const nodes = () => data.role === "server" ? [{ s: null, r: null, rep: { url: location.host, role: data.server_role, takes_writes: data.server_role !== "replica", scope: data.cluster } }]
    : data.shards.flatMap((sh, si) => sh.replicas.map((rep, ri) => ({ s: si, r: ri, rep })));
  /** Writes and reads a minute of a scope, over the window Tables and storage uses: {w, r, win} or null (a copy, or not measured yet). */
  function rateOf(cur) {
    const rt = data?.rates;
    if (!rt || rt.window_s == null || cur.kind === "replica") return null;
    const x = cur.kind === "shard" ? rt.shards?.[String(cur.shard)] : rt;
    return x ? { w: x.writes_per_min, r: x.reads_per_min, win: rt.window_s < 90 ? `last ${rt.window_s} s` : `last ${Math.round(rt.window_s / 60)} min` } : null;
  }
  function current() {
    if (scope.kind === "shard" && data.shards[scope.s]) return { sc: data.shards[scope.s].scope, ns: nodes().filter((n) => n.s === scope.s), name: `shard ${data.shards[scope.s].shard}`, kind: "shard", shard: data.shards[scope.s].shard };
    if (scope.kind === "replica" && data.shards[scope.s]?.replicas[scope.r]) {
      const rep = data.shards[scope.s].replicas[scope.r];
      return { sc: rep.scope, error: rep.error, ns: [{ s: scope.s, r: scope.r, rep }], name: copyName(rep.url), kind: "replica" };
    }
    scope = { kind: "cluster" };
    return { sc: data.cluster, ns: nodes(), name: data.role === "server" ? "this server" : "the whole cluster", kind: "cluster" };
  }
  const choose = (sc) => { scope = sc; store.set("inspector.scope", sc); draw(); };
  const same = (a) => a.kind === scope.kind && a.s === scope.s && a.r === scope.r;
  const button = (cls, sc, kids, label) => h("div", { class: cls + (same(sc) ? " sel" : ""), role: "button", tabindex: 0, "aria-pressed": String(same(sc)), "aria-label": label, onclick: (e) => { e.stopPropagation(); choose(sc); }, onkeydown: (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); choose(sc); } } }, kids);
  const live = (sc) => sc?.stats?.docs?.live ?? 0;

  /** A copy's state: bad (no answer), warn (behind its primary), ok. */
  const repState = (rep) => rep.error ? "bad" : rep.behind ? "warn" : "ok";
  const lagOf = (rep) => lag?.get(copyName(rep.url)) || lag?.get(rep.url) || null;
  const behindText = (rep) => { const l = lagOf(rep); return `${fmt.plural(rep.behind, "write")}${l?.behind_s != null ? `, ${fmt.dur(l.behind_s)}` : ""} behind the primary`; };
  function repCard(si, ri, rep) {
    const rs = rep.scope?.stats;
    const state = repState(rep);
    const when = rep.error ? rep.error : rep.behind ? behindText(rep) : rep.scope?.freshness?.last_write_ms ? `written ${fmt.ago(rep.scope.freshness.last_write_ms)}` : "no write since it started";
    return button("ix-rep", { kind: "replica", s: si, r: ri }, [
      h("div", { class: "row nw", style: { gap: "6px" } }, h("span", { class: "dot " + state }), h("b", { class: "mono", title: copyTitle(rep.url) }, copyName(rep.url)), chip(rep.takes_writes ? "prim" : "", rep.takes_writes ? "primary" : rep.role || "replica"), rep.reads_on === false ? chip("", "reads not counted") : null),
      h("span", { class: "tiny muted ix-n" }, rs ? `${fmt.n(rs.docs.live)} docs · ${fmt.bytes(rs.bytes.total)} · ${fmt.plural(rs.segments.length, "segment")}` : "–"),
      h("span", { class: "tiny ix-n " + (rep.error ? "ix-bad" : "muted"), title: when }, when)], copyName(rep.url));
  }

  function drawMap() {
    const c = data.cluster, st = c.stats;
    const copies = nodes().length;
    const summary = st ? `${fmt.n(st.docs.live)} rows of ${data.table} · ${fmt.bytes(st.bytes.total)} of data${c.copies_bytes > st.bytes.total ? `, ${fmt.bytes(c.copies_bytes)} with every copy` : ""}` : "no statistics";
    const top = button("ix-cluster", { kind: "cluster" }, [h("strong", {}, data.role === "server" ? "This server" : "Whole cluster"), h("span", { class: "small muted ix-n" }, data.role === "server" ? `${data.server_role || "server"} · ${summary}` : `${fmt.plural(data.shards.length, "shard")} · ${fmt.plural(copies, "copy", "copies")} · ${summary}`)], "Whole cluster");
    if (data.role === "server") { clear(map, top); return; }
    if (data.shards.length <= MANY) {
      clear(map, top, h("div", { class: "ix-shards" }, data.shards.map((sh, si) => {
        const ss = sh.scope.stats;
        return h("div", { class: "ix-shard" + (scope.kind === "shard" && scope.s === si ? " sel" : "") },
          button("ix-shard-hd", { kind: "shard", s: si }, [h("strong", {}, `Shard ${sh.shard}`), h("span", { class: "small muted ix-n" }, ss ? `${fmt.n(ss.docs.live)} docs · ${fmt.bytes(ss.bytes.total)}` : "no copy answered")], `Shard ${sh.shard}`),
          h("div", { class: "ix-reps" }, sh.replicas.map((rep, ri) => repCard(si, ri, rep))));
      })));
      return;
    }
    // At scale: the copies with a problem first, the shards as a grid, the
    // copies of the shard picked; never every copy at once.
    const probs = nodes().filter((n) => n.rep.error || n.rep.behind).sort((a, b) => (b.rep.error ? 1 : 0) - (a.rep.error ? 1 : 0) || (b.rep.behind || 0) - (a.rep.behind || 0));
    const worst = (sh) => sh.replicas.some((x) => x.error) ? "bad" : sh.replicas.some((x) => x.behind) ? "warn" : "ok";
    const counts = { bad: 0, warn: 0, ok: 0 };
    for (const sh of data.shards) counts[worst(sh)]++;
    const sel = scope.kind !== "cluster" ? scope.s : null;
    const selShard = sel != null ? data.shards[sel] : null;
    clear(map, top,
      probs.length ? h("div", { class: "stack", style: { gap: "6px" } }, h("div", { class: "small" }, h("b", {}, `${fmt.plural(probs.length, "copy", "copies")} of ${fmt.n(copies)} with a problem`), h("span", { class: "muted" }, ", worst first")),
        h("div", { class: "ix-reps" }, probs.slice(0, 8).map((n) => repCard(n.s, n.r, n.rep))), probs.length > 8 ? h("div", { class: "small muted" }, `and ${probs.length - 8} more: they are marked in the grid below`) : null)
        : h("div", { class: "small muted" }, `All ${fmt.n(copies)} copies answered and are current.`),
      h("div", { class: "stack", style: { gap: "6px" } },
        h("div", { class: "small" }, h("b", {}, `${fmt.n(data.shards.length)} shards`), h("span", { class: "muted" }, ` · ${fmt.n(counts.ok)} current${counts.warn ? ` · ${fmt.n(counts.warn)} with a copy behind` : ""}${counts.bad ? ` · ${fmt.n(counts.bad)} with a copy not answering` : ""} · pick one for its copies`)),
        h("div", { class: "dt-shardgrid", role: "group", "aria-label": "Shards" }, data.shards.map((sh, si) => button("dt-sh " + worst(sh), { kind: "shard", s: si }, String(sh.shard), `Shard ${sh.shard}`)))),
      selShard ? h("div", { class: "ix-shard sel" },
        button("ix-shard-hd", { kind: "shard", s: sel }, [h("strong", {}, `Shard ${selShard.shard}`), h("span", { class: "small muted ix-n" }, selShard.scope.stats ? `${fmt.n(selShard.scope.stats.docs.live)} docs · ${fmt.bytes(selShard.scope.stats.bytes.total)}` : "no copy answered")], `Shard ${selShard.shard}`),
        h("div", { class: "ix-reps" }, selShard.replicas.map((rep, ri) => repCard(sel, ri, rep)))) : null);
  }

  // -------------------------------------------------------------- tiles
  function drawTiles(cur) {
    const { sc, ns } = cur;
    const rate = rateOf(cur);
    const st = sc.stats, f = sc.freshness, rd = sc.reads;
    const tile = (k, v, sub, t) => h("div", { class: "card kpi ix-tile", title: t }, h("div", { class: "lbl" }, k), h("div", { class: "val ix-n" }, v), h("div", { class: "small dim ix-tsub" }, sub));
    const counts = {};
    for (const x of st.fields) counts[GROUP[x.kind] || x.kind] = (counts[GROUP[x.kind] || x.kind] || 0) + 1;
    const keyed = st.fields.filter((x) => x.key).length;
    const late = ns.filter((n) => n.rep.behind).sort((a, b) => b.rep.behind - a.rep.behind)[0];
    const docsTotal = st.docs.live + st.docs.deleted;
    clear(tiles,
      tile("Documents", fmt.n(st.docs.live), `${fmt.n(st.docs.deleted)} deleted (${fmt.pct(docsTotal ? st.docs.deleted / docsTotal : 0)}) · ${fmt.n(st.docs.in_memory)} in memory`),
      tile("Fields", fmt.n(st.fields.length), [keyed ? `${keyed} key` : null, ...Object.entries(counts).map(([k, n]) => `${n} ${k}`)].filter(Boolean).join(" · ")),
      tile("On disk", fmt.bytes(st.bytes.total), `${fmt.plural(st.segments.length, "segment")}${sc.copies_bytes > st.bytes.total ? ` · ${fmt.bytes(sc.copies_bytes)} with every copy` : ""}${st.bytes.in_memory ? ` · ${fmt.bytes(st.bytes.in_memory)} in memory` : ""}`),
      tile("Per document", fmt.bytes(st.docs.live ? st.bytes.total / st.docs.live : 0), `index and store · ${fmt.bytes(st.doc_bytes.mean)} stored raw (sampled)`),
      tile("Last write", f.last_write_ms ? fmt.ago(f.last_write_ms) : "none", late ? h("a", { href: "#", onclick: (e) => { e.preventDefault(); choose({ kind: "replica", s: late.s, r: late.r }); } }, `${copyName(late.rep.url)} is ${behindText(late.rep)}`) : f.last_write_ms ? (rate?.w != null ? `${fmt.si(rate.w)} writes/min over the ${rate.win} · ${fmt.n(f.writes)} since start` : `${fmt.n(f.writes)} writes since start${cur.kind === "replica" ? "" : " · a minute rate shows once the server measured one"}`) : `since the server started${f.modified_ms ? ` · file changed ${fmt.ago(f.modified_ms)}` : ""}`, f.last_write_ms ? fmt.datetime(f.last_write_ms) : null),
      tile(h("span", { class: "row nw", style: { gap: "6px" } }, "Retrieved", approx()), rd.docs.total ? fmt.pct(rd.docs.coverage ?? 0) : "–", rd.docs.total ? `of documents at least once since each server started · ${fmt.si(rd.docs.total)} retrievals, ${fmt.si(rd.keys.total)} key reads` : data.reads_on === false ? "read counting is off (--no-read-stats)" : `no reads counted since each server started · ${fmt.si(rd.keys.total)} key reads`));
  }

  // ----------------------------------------------------------- treemap
  function drawBytes({ sc }) {
    const st = sc.stats, items = [];
    let shares = 0;
    for (const x of st.fields) {
      if (x.index_bytes > 0) items.push({ label: x.name, sub: "index", v: x.index_bytes, c: KIND[x.kind] || OTHER, f: x });
      if (x.store_bytes > 0.5) { items.push({ label: x.name, sub: "stored", v: x.store_bytes, c: STORE, f: x }); shares += x.store_bytes; }
    }
    if (st.bytes.store - shares > 1) items.push({ label: "doc store", sub: "framing", v: st.bytes.store - shares, c: STORE });
    for (const [k, v] of Object.entries(st.bytes.other || {})) if (v > 0) items.push({ label: k === "pk" ? "primary key" : k, sub: "", v, c: OTHER });
    if (st.bytes.overhead > 0) items.push({ label: "segment overhead", sub: "", v: st.bytes.overhead, c: OVER });
    if (!items.length) { clear(bytes, empty("layers", "No segments yet", "Rows wait in memory until they are flushed.")); return; }
    items.sort((a, b) => b.v - a.v);
    const W = 640, H = 280, total = items.reduce((a, x) => a + x.v, 0), rects = [];
    (function lay(list, x, y, w, ht) {
      if (!list.length || w < 1 || ht < 1) return;
      const sum = list.reduce((a, i) => a + i.v, 0), short = Math.min(w, ht);
      const worst = (r) => { const t = r.reduce((a, i) => a + i.v, 0), side = t / sum * (w * ht) / short; return Math.max(...r.map((i) => { const len = i.v / t * short; return Math.max(side / len, len / side); })); };
      let row = [], best = Infinity, k = 0;
      for (; k < list.length; k++) { const next = [...row, list[k]], ww = worst(next); if (ww > best) break; row = next; best = ww; }
      const rs = row.reduce((a, i) => a + i.v, 0), side = rs / sum * (w * ht) / short;
      let off = 0;
      for (const i of row) { const len = i.v / rs * short; rects.push(w >= ht ? { ...i, x, y: y + off, w: side, h: len } : { ...i, x: x + off, y, w: len, h: side }); off += len; }
      if (w >= ht) lay(list.slice(k), x + side, y, w - side, ht); else lay(list.slice(k), x, y + side, w, ht - side);
    })(items, 0, 0, W, H);
    const svg = svgBox(W, H, "Bytes on disk by field");
    for (const r of rects) {
      const name = r.label + (r.sub ? " · " + r.sub : "");
      const g = s("g", { tabindex: 0 }, s("rect", { x: r.x + 1, y: r.y + 1, width: Math.max(0, r.w - 2), height: Math.max(0, r.h - 2), rx: 4, style: { fill: r.c, fillOpacity: 0.9 } }));
      if (r.w > name.length * 7 + 14 && r.h > 38) {
        const ink = "ix-on" + (LIGHT.has(r.c) ? " ink" : "");
        g.append(s("text", { x: r.x + 8, y: r.y + 18, class: ink }, name), s("text", { x: r.x + 8, y: r.y + 33, class: ink + " dim" }, `${fmt.bytes(r.v)} · ${fmt.pct(r.v / total)}`));
      }
      tipOn(g, () => tip(name, [[fmt.bytes(r.v), fmt.pct(r.v / total) + " of the table", r.c], ...(r.sub === "index" ? Object.entries(r.f.sections || {}).map(([p, b]) => [p, fmt.bytes(b)]) : []), ...(r.sub === "stored" ? [["its share of the compressed store, by raw bytes", ""]] : [])]));
      svg.append(g);
    }
    const kinds = new Set(items.map((i) => i.c));
    clear(bytes, svg, h("div", { class: "legend", style: { marginTop: "8px" } }, LEGEND.filter(([, c]) => kinds.has(c)).map(([k, c]) => h("span", { class: "ix-key", style: { "--c": c } }, k))),
      h("div", { class: "small muted" }, `${fmt.bytes(st.bytes.fields)} in field indexes · ${fmt.bytes(st.bytes.store)} doc store (${fmt.bytes(st.bytes.store_raw)} raw, ${st.bytes.store ? (st.bytes.store_raw / st.bytes.store).toFixed(1) : "–"}× compressed) · ${fmt.bytes(st.bytes.overhead)} overhead`));
  }

  // --------------------------------------------------------- histograms
  // A histogram of log-linear buckets [[lo, hi, weight]] on a log axis,
  // with percentile marks.
  function histogram(buckets, marks, { W = 460, H = 200, unit = fmt.bytes, what = "documents" } = {}) {
    const bs = buckets.filter((b) => b[2] > 0);
    if (!bs.length) return empty("info", "No values sampled");
    const pad = { l: 40, r: 12, t: 12, b: 26 };
    const L = (v) => Math.log2(Math.max(1, v));
    const lo = L(bs[0][0]), hi = Math.max(lo + 1, L(bs[bs.length - 1][1]));
    const X = (v) => pad.l + (L(v) - lo) / (hi - lo) * (W - pad.l - pad.r);
    // Weight per octave, so that wide and narrow buckets compare.
    const dens = bs.map(([a, b, c]) => c / Math.max(0.05, L(b) - L(Math.max(1, a))));
    const max = Math.max(...dens);
    const Y = (d) => H - pad.b - d / max * (H - pad.t - pad.b);
    const svg = svgBox(W, H, "Histogram");
    for (let i = 0; i <= 3; i++) { const y = Y(max * i / 3); svg.append(s("line", { x1: pad.l, x2: W - pad.r, y1: y, y2: y, class: "ix-grid-line" })); }
    bs.forEach(([a, b, c], i) => {
      const r = s("rect", { x: X(Math.max(1, a)) + 0.5, y: Y(dens[i]), width: Math.max(1.5, X(b) - X(Math.max(1, a)) - 1), height: H - pad.b - Y(dens[i]), rx: 1, class: "ix-bar", tabindex: 0 });
      tipOn(r, () => tip(`${unit(a)} – ${unit(b)}`, [[`${fmt.n(c)} ${what}`, ""]]));
      svg.append(r);
    });
    const step = Math.max(1, Math.ceil((hi - lo) / 6));
    for (let p = Math.ceil(lo); p <= hi; p += step) svg.append(s("text", { x: X(2 ** p), y: H - 8, "text-anchor": "middle" }, unit(2 ** p)));
    const ms = marks.filter(([, v]) => v != null);
    ms.forEach(([name, v]) => svg.append(tipOn(s("line", { x1: X(v), x2: X(v), y1: pad.t, y2: H - pad.b, class: "ix-mark", tabindex: 0 }), () => tip(name, [[unit(v), ""]]))));
    return h("div", {}, h("div", { class: "ix-marks small" }, ms.map(([name, v]) => h("span", {}, h("span", { class: "muted" }, name + " "), h("b", { class: "ix-n" }, unit(v))))), svg);
  }

  function drawDocs({ sc }) {
    const st = sc.stats, d = st.doc_bytes;
    if (!d.sampled) { clear(docs, empty("info", "No stored values sampled", "Documents with no stored field, or none yet.")); return; }
    clear(docs, histogram(d.buckets, [["p50", d.p50], ["p90", d.p90], ["p99", d.p99]]),
      h("div", { class: "small muted", style: { marginTop: "6px" } }, `${fmt.n(st.sample.docs)} documents sampled of ${fmt.n(st.sample.of)} · mean ${fmt.bytes(d.mean)} · largest seen ${fmt.bytes(d.max)} · smallest ${fmt.bytes(d.min)}`));
  }

  // ------------------------------------------------------------- fields
  const isTime = (x) => x.kind === "timestamp";
  const showValue = (x, v) => v == null ? "–" : isTime(x) ? new Date(v / 1000).toISOString().slice(0, 16).replace("T", " ") : x.kind === "float" ? fmt.si(v, 2) : fmt.n(v);
  function spread(x) {
    const bs = (x.size?.buckets || []).filter((b) => b[2] > 0);
    if (bs.length < 2) return h("span", { class: "tiny muted" }, bs.length ? "fixed" : "–");
    const W = 90, H = 18, max = Math.max(...bs.map((b) => b[2]));
    const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H, class: "ix-spark" });
    const w = W / bs.length;
    bs.forEach(([, , c], i) => svg.append(s("rect", { x: i * w, y: H - 1 - (H - 2) * c / max, width: Math.max(1, w - 1), height: (H - 2) * c / max + 1, style: { fill: KIND[x.kind] || OTHER } })));
    return tipOn(svg, () => tip(`${x.name}: value sizes`, [["min", fmt.bytes(x.size.min)], ["p50", fmt.bytes(x.size.p50)], ["p90", fmt.bytes(x.size.p90)], ["p99", fmt.bytes(x.size.p99)], ["max", fmt.bytes(x.size.max)]]));
  }
  function drawFields({ sc }) {
    const st = sc.stats, n = Math.max(1, st.docs.live);
    const type = (x) => `${x.kind}${x.dim ? `(${x.dim})` : ""}${x.multi ? "[]" : ""}`;
    const cols = [
      { key: "name", label: "Field", render: (x) => h("span", { class: "row nw", style: { gap: "6px" } }, h("b", {}, x.name), x.key ? chip("prim", "key") : null, !x.stored && x.kind !== "vector" ? chip("", "not stored") : null) },
      { key: "kind", label: "Type", render: (x) => h("span", { class: "mono small" }, type(x)), sort: type },
      { key: "fill", label: "Filled", num: true, sort: (x) => x.present / n, render: (x) => tipOn(h("div", { class: "row nw", style: { gap: "8px", justifyContent: "flex-end" } }, h("div", { class: "barbox" }, h("div", { class: "bar", style: { width: Math.min(100, x.present / n * 100) + "%", background: KIND[x.kind] || OTHER } })), h("span", { class: "small num" }, fmt.pct(x.present / n))), () => tip(x.name, [[`≈ ${fmt.n(x.present)} documents have one`, ""], [x.present_from === "norms" ? "counted from every length norm" : "from the sample", ""]])) },
      { key: "p50", label: "Value p50 / p99", num: true, sort: (x) => x.size?.p50 ?? -1, render: (x) => h("div", {}, x.size?.sampled ? `${fmt.bytes(x.size.p50)} / ${fmt.bytes(x.size.p99)}` : "–", x.items?.sampled ? h("div", { class: "tiny muted" }, `${fmt.n(x.items.p50)} items, p99 ${fmt.n(x.items.p99)}`) : null) },
      { key: "spread", label: "Size spread", sortable: false, render: spread },
      { key: "tokens", label: "Tokens p50 / p99", num: true, sort: (x) => x.tokens?.p50 ?? -1, render: (x) => x.tokens?.n ? h("span", { title: `mean ${fmt.n(x.tokens.mean, 1)} · max ${fmt.n(x.tokens.max)}` }, `${fmt.n(x.tokens.p50)} / ${fmt.n(x.tokens.p99)}`) : h("span", { class: "muted" }, "–") },
      { key: "terms", label: "Distinct terms", num: true, sort: (x) => x.terms?.estimate ?? x.terms?.high ?? -1, render: (x) => x.terms ? h("span", { title: x.terms.low === x.terms.high ? "exact" : `between ${fmt.n(x.terms.low)} and ${fmt.n(x.terms.high)}${x.terms.estimate != null ? ", estimated from a sketch" : ""}` }, (x.terms.estimate != null && x.terms.low !== x.terms.high ? "≈ " : "") + fmt.si(x.terms.estimate ?? x.terms.high)) : h("span", { class: "muted" }, "–") },
      { key: "index_bytes", label: "Index", num: true, render: (x) => x.index_bytes ? fmt.bytes(x.index_bytes) : h("span", { class: "muted" }, "none") },
      { key: "store_bytes", label: "Stored", num: true, render: (x) => x.store_bytes > 0.5 ? fmt.bytes(x.store_bytes) : h("span", { class: "muted" }, x.stored ? "–" : "not stored") },
      { key: "range", label: "Range", sortable: false, render: (x) => x.min != null ? h("span", { class: "small num" }, `${showValue(x, x.min)} … ${showValue(x, x.max)}`) : h("span", { class: "muted" }, "–") },
    ];
    clear(fields, table(cols, { rows: st.fields, sortKey: "index_bytes", desc: true, compact: true }),
      h("div", { class: "small muted", style: { marginTop: "8px" } }, `Value sizes and fill from ${fmt.n(st.sample.docs)} sampled documents (fill of a field that is not stored from every length norm); index bytes and terms from every segment; tokens from every document's length norm; ranges from the columns' bounds.`));
  }

  // ---------------------------------------------------------- freshness
  function drawFresh(cur) {
    const { sc, ns: all } = cur;
    const rate = rateOf(cur);
    const f = sc.freshness, now = data.at_ms;
    // At scale a lane a copy is thousands of pixels: the problems and the
    // copies written longest ago, the rest summed up in a line.
    const LANES = 12;
    let ns = all, rest = null;
    if (all.length > LANES) {
      const lastOf = (n) => n.rep.scope?.freshness?.last_write_ms || 0;
      const probs = all.filter((n) => n.rep.error || n.rep.behind);
      const others = all.filter((n) => !n.rep.error && !n.rep.behind).sort((a, b) => lastOf(a) - lastOf(b));
      ns = [...probs, ...others].slice(0, LANES);
      const left = all.filter((n) => !ns.includes(n));
      const ws = left.map(lastOf).filter(Boolean);
      rest = left.length ? `${fmt.plural(left.length, "other copy", "other copies")}: current${ws.length ? `, last writes between ${fmt.ago(Math.min(...ws))} and ${fmt.ago(Math.max(...ws))}` : ""}.` : null;
    }
    const ages = ns.map((n) => n.rep.scope?.freshness?.last_write_ms).filter(Boolean).map((t) => now - t);
    const SPANS = [60e3, 5 * 60e3, 15 * 60e3, 3600e3, 6 * 3600e3, 86400e3, 7 * 86400e3, 30 * 86400e3, 365 * 86400e3];
    const span = SPANS.find((x) => x >= Math.max(...ages, 1) * 1.1) || SPANS[SPANS.length - 1];
    const W = 540, lane = 36, H = 26 + ns.length * lane, x0 = 170;
    const X = (age) => x0 + (1 - Math.min(1, age / span)) * (W - x0 - 16);
    const svg = svgBox(W, H, "Last write of each copy");
    for (let i = 0; i <= 4; i++) {
      const age = span * i / 4, x = X(age);
      svg.append(s("line", { x1: x, x2: x, y1: 6, y2: H - 18, class: "ix-grid-line" }), s("text", { x, y: H - 4, "text-anchor": "middle" }, i ? fmt.ago(now - age).replace(" ago", "") + " ago" : "now"));
    }
    ns.forEach((n, i) => {
      const y = 18 + i * lane, fr = n.rep.scope?.freshness, stx = n.rep.scope?.stats;
      const who = n.s == null ? (n.rep.role || "server") : `${copyName(n.rep.url)} · ${n.rep.takes_writes ? "primary" : n.rep.role || "replica"}`;
      const l = n.rep.behind ? lagOf(n.rep) : null;
      svg.append(s("text", { x: 0, y: y + 3, class: "ix-lbl" }, who), s("text", { x: 0, y: y + 16 }, n.rep.error ? "no answer" : n.rep.behind ? `${fmt.plural(n.rep.behind, "write")}${l?.behind_s != null ? `, ${fmt.dur(l.behind_s)}` : ""} behind` : stx?.seq_no != null ? `seq ${fmt.n(stx.seq_no)}` : `txn ${fmt.n(stx?.txn)}`));
      svg.append(s("line", { x1: X(span), x2: X(0), y1: y, y2: y, class: "ix-lane" }));
      if (fr?.modified_ms && now - fr.modified_ms <= span) svg.append(tipOn(s("circle", { cx: X(now - fr.modified_ms), cy: y, r: 4, class: "ix-file", tabindex: 0 }), () => tip("file last changed", [[fmt.datetime(fr.modified_ms), fmt.ago(fr.modified_ms)]])));
      if (fr?.last_write_ms) svg.append(tipOn(s("circle", { cx: X(now - fr.last_write_ms), cy: y, r: 6, class: n.rep.behind ? "ix-behind" : "ix-write", tabindex: 0 }), () => tip("last write", [[fmt.datetime(fr.last_write_ms), fmt.ago(fr.last_write_ms)], ["writes since start", fmt.n(fr.writes)]])));
      else svg.append(s("text", { x: X(span / 2), y: y - 6, "text-anchor": "middle" }, n.rep.error ? n.rep.error.slice(0, 60) : "no write since the server started"));
    });
    const times = sc.stats.fields.filter((x) => isTime(x) && x.max != null).sort((a, b) => b.max - a.max)[0];
    clear(fresh, svg, h("div", { class: "ix-facts small" },
      h("span", {}, h("span", { class: "muted" }, "last write "), f.last_write_ms ? fmt.datetime(f.last_write_ms) : "none since start"),
      h("span", {}, h("span", { class: "muted" }, "writes since start "), fmt.n(f.writes)),
      rate?.w != null ? h("span", {}, h("span", { class: "muted" }, `writes/min, ${rate.win} `), fmt.si(rate.w)) : null,
      rate?.r != null ? h("span", {}, h("span", { class: "muted" }, `reads/min, ${rate.win} `), fmt.si(rate.r)) : null,
      h("span", {}, h("span", { class: "muted" }, "rows in memory "), fmt.n(sc.stats.docs.in_memory)),
      times ? h("span", {}, h("span", { class: "muted" }, `newest ${times.name} `), showValue(times, times.max)) : null,
      f.started_ms ? h("span", {}, h("span", { class: "muted" }, "counting since "), fmt.datetime(f.started_ms)) : null),
      rest ? h("div", { class: "small muted", style: { marginTop: "4px" } }, rest) : null,
      h("div", { class: "tiny muted", style: { marginTop: "4px" } }, "Filled dots: each copy's last write; rings: when its file last changed."));
  }

  // ------------------------------------------------------------- shards
  function drawSkew() {
    if (data.role === "server" || data.shards.length < 2) { clear(skew, empty("topology", data.role === "server" ? "One server" : "One shard", "Open a table on a coordinator over two or more shards to compare them.")); return; }
    const per = data.shards.map((sh, i) => ({ i, name: `s${sh.shard}`, shard: sh.shard, docs: sh.scope.stats?.docs.live || 0, bytes: sh.scope.stats?.bytes.total || 0, retr: sh.scope.reads.docs.total, keys: sh.scope.reads.keys.total }));
    const rows = [["Documents", "docs", fmt.si], ["Bytes on disk", "bytes", fmt.bytes], ["Retrievals since start", "retr", fmt.si], ["Key reads since start", "keys", fmt.si]];
    const many = per.length > MANY;
    const W = 540, H = rows.length * 48 + 4, svg = svgBox(W, H, "Shards compared");
    rows.forEach(([name, k, f], ri) => {
      const y = 4 + ri * 48, tot = per.reduce((a, p) => a + p[k], 0);
      svg.append(s("text", { x: 0, y: y + 12, class: "ix-lbl" }, name));
      if (!tot) { svg.append(s("text", { x: W, y: y + 12, "text-anchor": "end" }, "none yet"), s("rect", { x: 0, y: y + 18, width: W, height: 18, rx: 3, class: "ix-empty" })); return; }
      const mean = tot / per.length;
      const big = per.reduce((b, p) => p[k] > b[k] ? p : b);
      const over = big[k] / mean - 1;
      svg.append(s("text", { x: W, y: y + 12, "text-anchor": "end" }, over > 0.005 ? `largest: shard ${big.shard}, ${fmt.pct(over)} over the mean` : "even"));
      if (many) {
        // Largest first, one bar a shard against the mean; the outliers in color.
        const sorted = [...per].sort((a, b) => b[k] - a[k]), bw = W / sorted.length, max = sorted[0][k] || 1;
        svg.append(s("line", { x1: 0, x2: W, y1: y + 36 - 18 * mean / max, y2: y + 36 - 18 * mean / max, class: "dt-mean" }));
        sorted.forEach((p, j) => {
          const ht = Math.max(1, 18 * p[k] / max), out = Math.abs(p[k] / mean - 1) > 0.25;
          const r = s("rect", { x: j * bw, y: y + 36 - ht, width: Math.max(0.8, bw - (bw > 3 ? 1 : 0)), height: ht, class: "dt-bar" + (p[k] / mean > 1.25 ? " hi" : p[k] / mean < 0.75 ? " lo" : ""), tabindex: out ? 0 : -1, style: { cursor: "pointer" }, onclick: () => choose({ kind: "shard", s: p.i }) });
          tipOn(r, () => tip(`shard ${p.shard}`, [[name, f(p[k])], ["against the mean", `${(p[k] / mean).toFixed(2)}×`]]));
          svg.append(r);
        });
        return;
      }
      let x = 0;
      per.forEach((p) => {
        const w = p[k] / tot * W, dim = scope.kind !== "cluster" && scope.s !== p.i, isBig = p === big && over > 0.25;
        const r = s("rect", { x: x + 1, y: y + 18, width: Math.max(0, w - 2), height: 18, rx: 3, tabindex: 0, class: "dt-bar" + (isBig ? " hi" : ""), style: { fillOpacity: dim ? 0.3 : 0.9, cursor: "pointer" }, onclick: () => choose({ kind: "shard", s: p.i }) });
        tipOn(r, () => tip(`shard ${p.shard}`, [[name, f(p[k])], ["share", fmt.pct(p[k] / tot)]]));
        svg.append(r);
        if (w > 80) svg.append(s("text", { x: x + 8, y: y + 31, class: "ix-on" }, `${p.name} ${f(p[k])}`));
        x += w;
      });
    });
    clear(skew, svg, many ? h("div", { class: "tiny muted" }, `Each bar is a shard, largest first; the dashed line is the mean. Orange: over 1.25× the mean. Click a bar for that shard.`) : null);
  }

  // -------------------------------------------------------------- reads
  function drawReads({ sc }) {
    const rd = sc.reads[what], n = live(sc);
    clear(whatSeg, seg([["docs", "Documents"], ["keys", "Keys"]], what, (v) => { what = v; store.set("inspector.what", v); draw(); }, { "aria-label": "Documents retrieved or keys asked for" }));
    const noun = what === "docs" ? "retrievals" : "key reads";
    if (!rd.total) {
      const why = data.reads_on === false ? "Read counting is off on this server (--no-read-stats)." : what === "docs" ? "Rows returned by queries count here: run a few searches." : "Reads by key count here: GET /v1/kv/<table>/<key>, POST /v1/kv/get, or Get over gRPC.";
      for (const el of [cover, curve, freq]) clear(el, empty("eye", `No ${noun} counted yet`, why));
    } else {
      // The ring: share of the documents read at least once.
      const cov = rd.coverage ?? 0, R = 66, C = 2 * Math.PI * R;
      const ring = s("svg", { viewBox: "0 0 200 170", width: "100%", class: "ix-ring", role: "img", "aria-label": `${fmt.pct(cov)} read at least once` },
        s("circle", { cx: 100, cy: 84, r: R, class: "ix-ring-bg" }),
        s("circle", { cx: 100, cy: 84, r: R, class: "ix-ring-fg", "stroke-dasharray": `${C * Math.min(1, cov)} ${C}`, transform: "rotate(-90 100 84)" }),
        s("text", { x: 100, y: 88, "text-anchor": "middle", class: "ix-lbl ix-big" }, fmt.pct(cov)),
        s("text", { x: 100, y: 106, "text-anchor": "middle" }, `of ${fmt.si(n)} documents`));
      const est = rd.exact ? "" : "≈ ";
      clear(cover, ring, h("dl", { class: "ix-dl small" },
        h("dt", {}, noun), h("dd", { class: "ix-n" }, fmt.n(rd.total)),
        h("dt", {}, what === "docs" ? "in answers" : "requests"), h("dd", { class: "ix-n" }, fmt.n(rd.answers)),
        h("dt", {}, `distinct ${what === "docs" ? "documents" : "keys"}`), h("dd", { class: "ix-n" }, est + fmt.n(rd.distinct)),
        h("dt", {}, "never read"), h("dd", { class: "ix-n" }, est + fmt.n(Math.max(0, n - rd.distinct))),
        what === "keys" ? [h("dt", {}, "keys not found"), h("dd", { class: "ix-n" }, `${fmt.n(rd.missing)} (${fmt.pct(rd.missing / rd.total)})`)] : null,
        rd.skipped ? [h("dt", {}, "past 10,000 rows of an answer"), h("dd", { class: "ix-n", title: "Rows past the first 10,000 of an answer count in the totals only" }, fmt.n(rd.skipped))] : null,
        rd.unkeyed ? [h("dt", {}, "rows with no key"), h("dd", { class: "ix-n" }, fmt.n(rd.unkeyed))] : null),
        h("div", { class: "tiny muted" }, rd.exact ? `Every key read so far is in the sample (${fmt.n(rd.sample.keys)}).` : `From a 1 in ${fmt.n(2 ** rd.sample.level)} hash sample of the keys (${fmt.n(rd.sample.keys)} kept).`));

      // The concentration curve.
      const W = 260, H = 200, X = (p) => 36 + p * 214, Y = (p) => 176 - p * 164;
      const L = svgBox(W, H, "Concentration of reads");
      for (let i = 0; i <= 4; i++) L.append(s("line", { x1: X(0), x2: X(1), y1: Y(i / 4), y2: Y(i / 4), class: "ix-grid-line" }), s("text", { x: 30, y: Y(i / 4) + 4, "text-anchor": "end" }, `${i * 25}%`), s("text", { x: X(i / 4), y: 194, "text-anchor": "middle" }, `${i * 25}%`));
      L.append(s("line", { x1: X(0), y1: Y(0), x2: X(1), y2: Y(1), class: "ix-even" }));
      const pts = rd.curve || [];
      if (pts.length) L.append(s("path", { d: pts.map(([x, y], i) => `${i ? "L" : "M"}${X(x).toFixed(1)},${Y(y).toFixed(1)}`).join(""), class: "ix-curve" }));
      if (rd.top10 != null) L.append(s("circle", { cx: X(0.1), cy: Y(rd.top10), r: 4, class: "ix-dot" }), s("text", { x: X(0.1) + 9, y: Y(rd.top10) + 4, class: "ix-lbl" }, `top 10%: ${fmt.pct(rd.top10)} of reads`));
      clear(curve, L, h("div", { class: "tiny muted" }, `The most read 1% of the ${what === "docs" ? "documents" : "keys"} read take ${fmt.pct(rd.top1 ?? 0)} of the ${noun}. Across: those read, most read first; up: their share. The diagonal is reads spread evenly.`));

      // How many times each was read.
      const BINS = [[1, 2, "1"], [2, 3, "2"], [3, 5, "3–4"], [5, 9, "5–8"], [9, 17, "9–16"], [17, 65, "17–64"], [65, Infinity, "65+"]];
      const pk = rd.per_key, sums = BINS.map(() => 0);
      for (const [lo, , w] of pk.buckets || []) { const i = BINS.findIndex(([a, b]) => lo >= a && lo < b); if (i >= 0) sums[i] += w; }
      const all = sums.reduce((a, x) => a + x, 0) || 1, top = Math.max(...sums) / all || 1, bw = 214 / BINS.length;
      const F = svgBox(W, H, "Times each was read");
      BINS.forEach(([, , name], i) => {
        const v = sums[i] / all, ht = v / top * 148;
        const r = s("rect", { x: 36 + i * bw + 2, y: 176 - ht, width: bw - 4, height: Math.max(0, ht), rx: 2, class: "ix-bar2", tabindex: 0 });
        tipOn(r, () => tip(`read ${name} ${name === "1" ? "time" : "times"}`, [[`${est}${fmt.n(sums[i])} ${what === "docs" ? "documents" : "keys"}`, fmt.pct(v)]]));
        F.append(r, s("text", { x: 36 + i * bw + bw / 2, y: 194, "text-anchor": "middle" }, name));
        if (v > 0) F.append(s("text", { x: 36 + i * bw + bw / 2, y: 171 - ht, "text-anchor": "middle" }, fmt.pct(v, 0)));
      });
      clear(freq, F, h("div", { class: "tiny muted" }, `Share of the ${what === "docs" ? "documents" : "keys"} read, by how many times each was. Mean ${fmt.n(pk.mean, 1)}, p99 ${fmt.n(pk.p99)}.`));
    }

    const topTable = (r, noun2) => {
      if (!r.top?.length) return empty("eye", `No ${noun2} counted yet`);
      // A count within its error says nothing ("3 ± 1"): left out.
      const solid = r.top.filter((x) => x.count - (x.err || 0) >= 2);
      if (!solid.length) return empty("eye", "Nothing stands out yet", `Every count is within its error (Space-Saving keeps ${fmt.n(r.top.length)} candidates): no ${noun2.replace(/s$/, "")} is read much more than the rest.`);
      return h("div", {}, table([
        { key: "key", label: "Key", render: (x) => h("span", { class: "mono small" }, x.key) },
        { key: "count", label: "Times", num: true, render: (x) => fmt.n(x.count) },
        { key: "err", label: "Error", num: true, render: (x) => x.err ? `± ${fmt.n(x.err)}` : h("span", { class: "muted" }, "exact") },
        { key: "share", label: `Share of ${noun2}`, num: true, render: (x) => h("div", { class: "row nw", style: { gap: "8px", justifyContent: "flex-end" } }, h("div", { class: "barbox" }, h("div", { class: "bar", style: { width: Math.min(100, x.share * 100 / Math.max(1e-9, r.top[0].share)) + "%" } })), h("span", { class: "small num" }, fmt.pct(x.share, 2))) },
      ], { rows: solid, sortKey: "count", desc: true, compact: true, maxHeight: "360px" }),
      solid.length < r.top.length ? h("div", { class: "tiny muted", style: { marginTop: "6px" } }, `${fmt.n(r.top.length - solid.length)} more left out: their counts are within their error.`) : null);
    };
    clear(topDocs, topTable(sc.reads.docs, "retrievals"));
    clear(topKeys, topTable(sc.reads.keys, "key reads"), sc.reads.keys.total ? h("div", { class: "small muted", style: { marginTop: "6px" } }, `${fmt.n(sc.reads.keys.missing)} of ${fmt.n(sc.reads.keys.total)} asked for a key no row has (${fmt.pct(sc.reads.keys.missing / sc.reads.keys.total)}).`) : null);
  }

  function draw() {
    hideTip();
    if (err) { clear(map, failure(err, "The index statistics")); clear(tiles); body.hidden = true; return; }
    if (!data) { clear(map, empty("layers", "Reading every segment…")); clear(tiles); body.hidden = true; return; }
    drawMap();
    const cur = current();
    drawSkew();
    if (!cur.sc?.stats) {
      clear(tiles, h("div", { class: "error", style: { gridColumn: "1 / -1" } }, cur.error ? `${cur.name}: ${cur.error}` : `No statistics for ${cur.name}: no copy answered.`));
      body.hidden = true;
      return;
    }
    body.hidden = false;
    readsTail.textContent = readsFrom();
    const f = Array.isArray(data.reads_from) ? data.reads_from : [], missing = f.filter((x) => !x.ok);
    partial.hidden = !missing.length;
    if (missing.length) clear(partial, h("div", { class: "note-warn small" }, icon("warn"), `${f.length - missing.length} of ${f.length} coordinators answered: ${missing.map((x) => x.name || x.url).join(", ")} did not, so the reads below leave out what went through ${missing.length === 1 ? "it" : "them"}.`));
    drawTiles(cur); drawBytes(cur); drawDocs(cur); drawFields(cur); drawFresh(cur); drawReads(cur);
  }

  draw();
  reopen = (params) => {
    want = { table: params.get("table"), shard: params.get("shard"), copy: params.get("copy") };
    if (want.table && tables.includes(want.table)) { pick.value = want.table; store.set("inspector.table", want.table); }
    load();
  };
  // A database with no tables yet: nothing to inspect until one is made.
  loadTables().then(() => {
    if (listed && !tables.length) { clear(content, empty("table", "No tables yet", "Import a file or create a table first.", h("a", { class: "btn primary", href: "#/import" }, icon("import"), "Import"))); stamp.textContent = ""; return; }
    load();
  });
  return () => { hideTip(); reopen = null; };
}
